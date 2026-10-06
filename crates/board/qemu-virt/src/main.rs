#![no_std]
#![no_main]

extern crate alloc;

mod uart;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use arch::{MemoryType, UserAccess, l1_block, user_page};
use dtb::Dtb;
use kernel::handle::Handles;
use kernel::syscall::{Call, EFAULT, ENOMEM};
use kernel::{FRAME_WORDS, Full, Memory, Program, Scheduler};
use linked_list_allocator::Heap;
use mm::{Budget, FrameAllocator, PhysAddr};
use uart::Uart;

/// Panic- and trap-path console; normal output uses the PL011 from the DTB.
const UART0: PhysAddr = PhysAddr(0x0900_0000);
/// QEMU loads the DTB at RAM base for an ELF kernel (x0 stays 0), if it fits below the image.
const DTB: PhysAddr = PhysAddr(0x4000_0000);
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const GIB: u64 = 1 << 30;
/// Outside both mapped GiBs.
const UNMAPPED: PhysAddr = PhysAddr(0x8000_0000);
/// Device memory in GiB 0 (MMIO), the kernel image, stack, heap and DTB in RAM in GiB 1; EL1-only, global.
const KERNEL_L1: [u64; 2] = [
    l1_block(PhysAddr(0), MemoryType::Device),
    l1_block(PhysAddr(GIB), MemoryType::Normal),
];
const PAGE: usize = 4096;
/// Where user programs' code is mapped; one 2 MiB region, so a process needs a single level-3 table.
const USER_BASE: u64 = 1 << 32;
/// Each process's one stack page ends here.
const USER_STACK_TOP: u64 = USER_BASE + (2 << 20);
/// Where a process's first `map` goes; later ones follow it.
const MAP_BASE: u64 = USER_STACK_TOP;
/// EL1 virtual timer PPI.
const TIMER_IRQ: u32 = 27;
const TICK_US: u64 = 10_000;

/// GIC CPU interface base, set before the first IRQ can be delivered.
static GIC_CPU: AtomicU64 = AtomicU64::new(0);
/// Boot context included; a task's slot is its ASID (8 bits).
const MAX_TASKS: usize = 8;
const _: () = assert!(MAX_TASKS <= 256);
/// Kernel stack per task: 16 KiB.
const TASK_STACK_FRAMES: usize = 4;

#[global_allocator]
static HEAP: KernelHeap = KernelHeap(UnsafeCell::new(Heap::empty()));

struct KernelHeap(UnsafeCell<Heap>);

// SAFETY: one core, and the heap is only touched with IRQs masked, so accesses never overlap.
unsafe impl Sync for KernelHeap {}

// SAFETY: `Heap` hands out non-overlapping blocks of at least `layout` from the region `init_heap` gave it.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let ptr = unsafe { &mut *self.0.get() }
            .allocate_first_fit(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr);
        arch::irq::restore(irq);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `GlobalAlloc` only passes pointers that `alloc` returned, which are non-null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let heap = unsafe { &mut *self.0.get() };
        // SAFETY: `ptr` was allocated from this heap with `layout`.
        unsafe { heap.deallocate(ptr, layout) }
        arch::irq::restore(irq);
    }
}

/// Task contexts and free frames; touched only with IRQs masked on the only core.
static KERNEL: Global = Global(UnsafeCell::new(Kernel {
    sched: Scheduler::new(),
    frames: FrameAllocator::empty(),
}));

struct Kernel {
    sched: Scheduler<MAX_TASKS>,
    frames: FrameAllocator<FRAME_WORDS>,
}

struct Global(UnsafeCell<Kernel>);

// SAFETY: one core, and the kernel state is only touched with IRQs masked, so accesses never overlap.
unsafe impl Sync for Global {}

/// # Safety
/// IRQs must be masked (trap context), so this is the only reference to the scheduler.
#[unsafe(no_mangle)]
unsafe extern "C" fn task_switch(frame: usize) -> usize {
    // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
    let sched = unsafe { &mut (*KERNEL.0.get()).sched };
    let (_, from) = sched.current();
    let next = sched.switch(frame);
    if sched.current().1 != from {
        // SAFETY: `frame` came from the trap path and `next` from the scheduler.
        unsafe { enter(sched, frame, next) };
    }
    next
}

/// Drops the current process, returns all its frames, and returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), the current task must be a process, and `frame` its trap frame.
unsafe fn task_exit(frame: usize) -> usize {
    // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
    let Kernel { sched, frames } = unsafe { &mut *KERNEL.0.get() };
    let (asid, l1) = sched.current();
    let (next, memory) = sched.exit();
    // SAFETY: `frame` is the exiting process's trap frame and `next` came from the scheduler.
    unsafe { enter(sched, frame, next) };
    arch::flush_asid(asid);
    // Frees the kernel stack this runs on: sound only while nothing allocates before the trap returns to `next`.
    // SAFETY: TTBR0 left `l1` above, and its tables hold only this process's frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    free_stack(frames, memory.stack);
    next
}

fn free_stack(frames: &mut FrameAllocator<FRAME_WORDS>, stack: PhysAddr) {
    for i in 0..TASK_STACK_FRAMES {
        frames.free(PhysAddr(stack.0 + (i * PAGE) as u64));
    }
}

/// A zeroed frame charged to `budget`.
fn zeroed(frames: &mut FrameAllocator<FRAME_WORDS>, budget: &mut Budget) -> Option<PhysAddr> {
    let page = budget.alloc(frames)?;
    // SAFETY: a fresh frame from the allocator: identity-mapped RAM that nothing else uses.
    unsafe { ptr::write_bytes(page.0 as *mut u8, 0, PAGE) };
    Some(page)
}

/// Maps a zeroed frame at `va` under `l1` with `access`, the frame and any new table charged to `budget`; `None`
/// (the frame refunded) if either is out of budget or frames.
fn map_zeroed(
    frames: &mut FrameAllocator<FRAME_WORDS>,
    budget: &mut Budget,
    l1: PhysAddr,
    va: u64,
    access: UserAccess,
) -> Option<PhysAddr> {
    let page = zeroed(frames, budget)?;
    let leaf = user_page(page, access);
    // SAFETY: `l1` is a process's table built from zeroed frames like these, and `va` is a user address it leaves unmapped;
    // map's `next` only grows, by at most the budget, and budgets stay within RAM, so `va` stays far below 512 GiB.
    if unsafe { arch::map_page(l1, va, leaf, || zeroed(frames, budget)) }.is_none() {
        budget.free(frames, page);
        return None;
    }
    Some(page)
}

/// Maps `pages` zeroed read-write pages at the current process's next map address, charged to its budget; returns
/// their address, or `None` with nothing mapped if the budget or the frames run out.
fn map(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pages: usize,
) -> Option<u64> {
    let (asid, l1) = sched.current();
    let memory = sched.memory();
    if pages > memory.budget.remaining() {
        return None;
    }
    let start = memory.next;
    let end = start + (pages * PAGE) as u64;
    for va in (start..end).step_by(PAGE) {
        if map_zeroed(frames, &mut memory.budget, l1, va, UserAccess::ReadWrite).is_none() {
            for va in (start..va).step_by(PAGE) {
                // SAFETY: this call mapped `va` under `l1` above.
                let page = unsafe { arch::unmap_page(l1, va) };
                memory.budget.free(frames, page);
            }
            arch::flush_asid(asid);
            return None;
        }
    }
    memory.next = end;
    Some(start)
}

/// Moves SP_EL0, TPIDR_EL0 and TTBR0 from the task that saved `frame` to the scheduler's current task, whose frame is
/// `next`: the boot table keeps ASID 0, a process's level-1 table has ASID = its slot.
///
/// # Safety
/// `frame` and `next` must be trap frames.
unsafe fn enter(sched: &Scheduler<MAX_TASKS>, frame: usize, next: usize) {
    let (slot, space) = sched.current();
    // SAFETY: the caller guarantees both are trap frames.
    unsafe { arch::switch_el0_regs(frame, next) };
    let (table, asid) = match space {
        PhysAddr(0) => (arch::boot_table(), 0),
        table => (table, slot),
    };
    // SAFETY: every space's table holds the kernel blocks, and ASID `slot` is used only by the task in that slot.
    unsafe { arch::set_ttbr0(table, asid) }
}

/// Builds a process for `code` (entered at `entry`): address space, pages and kernel stack, all charged to `budget`,
/// and queues it; on failure returns every frame it took.
fn spawn_process(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    code: &[u8],
    entry: u64,
    mut budget: Budget,
) -> Result<(), Full> {
    let l1 = zeroed(frames, &mut budget).ok_or(Full)?;
    // SAFETY: `l1` is a fresh, zeroed frame.
    unsafe { (l1.0 as *mut [u64; 2]).write(KERNEL_L1) };
    let stack = (|| {
        let text = map_zeroed(frames, &mut budget, l1, entry, UserAccess::ReadExecute)?;
        // SAFETY: `code` fits in the fresh frame `text` (checked by the caller).
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), text.0 as *mut u8, code.len()) };
        // SAFETY: `text` is identity-mapped RAM.
        unsafe { arch::sync_icache(text.0 as usize, PAGE) };
        let stack_page = USER_STACK_TOP - PAGE as u64;
        map_zeroed(frames, &mut budget, l1, stack_page, UserAccess::ReadWrite)?;
        budget.alloc_contiguous(frames, TASK_STACK_FRAMES)
    })();
    let added = stack.ok_or(Full).and_then(|stack| {
        // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new process.
        let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, USER_STACK_TOP) };
        let memory = Memory {
            stack: stack.start,
            budget,
            next: MAP_BASE,
        };
        let added = sched.add(frame, l1, memory, Handles::init);
        if added.is_err() {
            free_stack(frames, stack.start);
        }
        added
    });
    if added.is_err() {
        // SAFETY: no TTBR0 ever used `l1`, and its tables hold only frames taken above.
        unsafe { arch::free_space(l1, |f| frames.free(f)) };
    }
    added
}

global_asm!(include_str!("user.s"), USER_BASE = const USER_BASE);

unsafe extern "C" {
    static user_counter: u8;
    static user_counter_end: u8;
    static user_intruder: u8;
    static user_intruder_end: u8;
    static user_kernel_reader: u8;
    static user_kernel_reader_end: u8;
    static user_bench: u8;
    static user_bench_end: u8;
    static user_handles: u8;
    static user_handles_end: u8;
    static user_budget: u8;
    static user_budget_end: u8;
}

/// A user program's code and the address it is mapped and starts at.
fn user_program(program: Program) -> (&'static [u8], u64) {
    let (start, end, va) = match program {
        Program::Counter => (
            &raw const user_counter,
            &raw const user_counter_end,
            USER_BASE,
        ),
        Program::Intruder => (
            &raw const user_intruder,
            &raw const user_intruder_end,
            USER_BASE + PAGE as u64,
        ),
        Program::KernelReader => (
            &raw const user_kernel_reader,
            &raw const user_kernel_reader_end,
            USER_BASE + PAGE as u64,
        ),
        Program::SyscallBench => (&raw const user_bench, &raw const user_bench_end, USER_BASE),
        Program::Handles => (
            &raw const user_handles,
            &raw const user_handles_end,
            USER_BASE,
        ),
        Program::Budget => (
            &raw const user_budget,
            &raw const user_budget_end,
            USER_BASE,
        ),
    };
    // SAFETY: `user.s` places each program's bytes between its start and end labels in read-only data.
    let code = unsafe { slice::from_raw_parts(start, end as usize - start as usize) };
    (code, va)
}

/// Writes the `len` bytes at user address `ptr` to the console if EL0 may read all of them; false otherwise.
fn write_user(ptr: u64, len: usize) -> bool {
    let first_page = ptr & !(PAGE as u64 - 1);
    if !(first_page..ptr + len as u64)
        .step_by(PAGE)
        .all(arch::user_readable)
    {
        return false;
    }
    // SAFETY: EL0 may read every page of the range, so it is mapped in the current address space, which
    // stays loaded and unchanged until the trap returns (IRQs masked, one core).
    let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, len) };
    Uart::new(UART0).write(bytes);
    true
}

/// What a new task's first frame hands to `task_start`; lives at the top of its stack.
struct Start {
    board: QemuVirt,
    entry: fn(&mut QemuVirt, usize) -> !,
    arg: usize,
}

extern "C" fn task_start(start: usize) -> ! {
    // SAFETY: `spawn` wrote a `Start` here, above the task's stack, and only this task uses it.
    let start = unsafe { &mut *(start as *mut Start) };
    (start.entry)(&mut start.board, start.arg)
}

#[derive(Clone)]
struct QemuVirt {
    uart: Uart,
    /// GICv2 distributor and CPU interface.
    gic: (PhysAddr, PhysAddr),
    entry_us: u64,
}

impl kernel::Board for QemuVirt {
    type Console = Uart;

    fn console(&mut self) -> &mut Uart {
        &mut self.uart
    }

    fn exception_level(&self) -> u8 {
        let el: u64;
        // SAFETY: reading CurrentEL has no side effects.
        unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
        (el >> 2) as u8
    }

    fn breakpoint_self_test(&mut self) {
        arch::breakpoint_self_test()
    }

    fn enable_mmu(&mut self) {
        // SAFETY: called at boot with the MMU off, before any atomic RMW; MMIO is in GiB 0, and the image, stack and DTB are in RAM in GiB 1.
        unsafe { arch::enable_mmu(&KERNEL_L1, arch::MAIR) }
    }

    fn read_unmapped(&mut self) {
        // SAFETY: the address is unmapped, so the read takes a data abort, which panics instead of returning.
        unsafe { (UNMAPPED.0 as *const u64).read_volatile() };
    }

    fn init_heap(&mut self, region: Range<PhysAddr>) {
        let size = (region.end.0 - region.start.0) as usize;
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let heap = unsafe { &mut *HEAP.0.get() };
        assert!(heap.bottom().is_null(), "heap already initialized");
        // SAFETY: the heap is empty (checked above); `region` being unused, mapped RAM is the `Board::init_heap` contract the kernel upholds.
        unsafe { heap.init(region.start.0 as *mut u8, size) }
        arch::irq::restore(irq);
    }

    fn uptime_us(&self) -> u64 {
        arch::uptime_us() - self.entry_us
    }

    fn start_timer(&mut self) {
        let (dist, cpu) = self.gic;
        GIC_CPU.store(cpu.0, Relaxed);
        // SAFETY: the DTB's GICv2 registers, in the device-mapped GiB 0.
        unsafe { arch::gic::enable(dist, cpu, TIMER_IRQ) };
        arch::timer::arm(TICK_US);
    }

    fn idle(&mut self) {
        arch::irq::wait()
    }

    fn power_off(&mut self) -> ! {
        shutdown()
    }

    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full> {
        let board = self.clone();
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let Kernel { sched, frames } = unsafe { &mut *KERNEL.0.get() };
        let added = frames
            .alloc_contiguous(TASK_STACK_FRAMES)
            .ok_or(Full)
            .and_then(|stack| {
                let start = (stack.end.0 as usize - size_of::<Start>()) & !15;
                // SAFETY: `start` is 16-byte aligned and inside the fresh stack, which nothing else references.
                unsafe { (start as *mut Start).write(Start { board, entry, arg }) };
                // SAFETY: `start` is 16-byte aligned and the stack below it is fresh and owned by the new task.
                let frame = unsafe { arch::new_task(start, task_start, start) };
                let memory = Memory {
                    stack: stack.start,
                    budget: Budget::new(0),
                    next: 0,
                };
                let added = sched.add(frame, PhysAddr(0), memory, |_| Handles::new());
                if added.is_err() {
                    free_stack(frames, stack.start);
                }
                added
            });
        arch::irq::restore(irq);
        added
    }

    fn yield_now(&mut self) {
        arch::yield_now()
    }

    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>) {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        unsafe { (*KERNEL.0.get()).frames = frames };
        arch::irq::restore(irq);
    }

    fn free_frames(&self) -> usize {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let free = unsafe { &(*KERNEL.0.get()).frames }.free_count();
        arch::irq::restore(irq);
        free
    }

    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), Full> {
        let (code, entry) = user_program(program);
        assert!(code.len() <= PAGE, "user program over one page");
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let Kernel { sched, frames } = unsafe { &mut *KERNEL.0.get() };
        let added = spawn_process(sched, frames, code, entry, Budget::new(budget));
        arch::irq::restore(irq);
        added
    }

    fn tasks(&self) -> usize {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let count = unsafe { &(*KERNEL.0.get()).sched }.count();
        arch::irq::restore(irq);
        count
    }
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let entry_us = arch::uptime_us();
    arch::install_vectors();
    arch::timer::allow_user_counter();

    // SAFETY: RAM base is mapped RAM; we only read the 8-byte FDT header there.
    let header = unsafe { slice::from_raw_parts(DTB.0 as *const u8, 8) };
    let size = dtb::total_size(header).expect("no DTB at RAM base");
    // SAFETY: the magic matched, so QEMU loaded `size` bytes of DTB here and nothing writes them.
    let blob = unsafe { slice::from_raw_parts(DTB.0 as *const u8, size) };

    let image =
        PhysAddr(&raw const __kernel_start as u64)..PhysAddr(&raw const __kernel_end as u64);
    let dtb_range = DTB..PhysAddr(DTB.0 + size as u64);

    let dtb = Dtb::new(blob).expect("bad DTB");
    let uart = dtb.uart().expect("no PL011 in DTB");
    let gic = dtb.gic().expect("no GICv2 in DTB");

    kernel::run(
        &mut QemuVirt {
            uart: Uart::new(uart),
            gic,
            entry_us,
        },
        dtb,
        &[image, dtb_range],
    )
}

/// # Safety
/// IRQs must be masked (trap context), as `task_switch` requires.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_irq(frame: usize) -> usize {
    let cpu = PhysAddr(GIC_CPU.load(Relaxed));
    // SAFETY: IRQs are delivered only after `start_timer` stored the DTB's GIC CPU interface.
    let iar = unsafe { arch::gic::ack(cpu) };
    let tick = iar == TIMER_IRQ;
    if tick {
        arch::timer::arm(TICK_US);
    }
    // SAFETY: as above.
    unsafe { arch::gic::eoi(cpu, iar) };
    if !tick {
        return frame;
    }
    // SAFETY: the caller masked IRQs.
    unsafe { task_switch(frame) }
}

/// # Safety
/// IRQs must be masked (trap context), and `frame` the current process's.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_syscall(frame: &mut arch::TrapFrame) -> usize {
    // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
    let Kernel { sched, frames } = unsafe { &mut *KERNEL.0.get() };
    let x = &mut frame.x;
    x[0] = match kernel::syscall::dispatch(x[8], x.first_chunk().unwrap(), sched.handles()) {
        // SAFETY: the caller masked IRQs, and `frame` is the current process's.
        Ok(Call::Exit) => return unsafe { task_exit(frame as *mut arch::TrapFrame as usize) },
        Ok(Call::Write { ptr, len }) if write_user(ptr, len) => len as u64,
        Ok(Call::Write { .. }) => EFAULT as u64,
        Ok(Call::Map { pages }) => map(sched, frames, pages).unwrap_or(ENOMEM as u64),
        Ok(Call::Done(value)) => value,
        Err(error) => error as u64,
    };
    frame as *mut arch::TrapFrame as usize
}

/// # Safety
/// IRQs must be masked (trap context), and `frame` the current process's.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_user_fault(frame: usize, ec: u64, far: u64) -> usize {
    // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
    let (slot, _) = unsafe { &(*KERNEL.0.get()).sched }.current();
    let _ = writeln!(Uart::new(UART0), "fault: {slot} ec={ec:#x} far={far:#x}");
    // SAFETY: as above; `frame` is the current process's.
    unsafe { task_exit(frame) }
}

fn shutdown() -> ! {
    // SAFETY: PSCI SYSTEM_OFF via HVC is the power-off call on QEMU `virt` at EL1.
    unsafe { core::arch::asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(noreturn)) }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(Uart::new(UART0), "panic: {info}");
    shutdown()
}
