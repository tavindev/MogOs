#![no_std]
#![no_main]

extern crate alloc;

mod uart;
mod virtio_blk;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use arch::{MemoryType, UserAccess, l1_block, user_page};
use dtb::Dtb;
use kernel::console::Line;
use kernel::elf::{Elf, Segment};
use kernel::handle::{DUPLICATE, Handles, KILL, MAX_HANDLES, Object, READ, TRANSFER, WAIT, WRITE};
use kernel::mutex::Mutexes;
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{Call, EAGAIN, EBADF, EFAULT, ENFILE, ENOENT, ENOEXEC, ENOMEM, KILLED};
use kernel::{Event, FRAME_WORDS, Full, Memory, PRIORITIES, Program, Scheduler};
use linked_list_allocator::Heap;
use mm::{Budget, FrameAllocator, PhysAddr};
use uart::Uart;
use virtio_blk::VirtioBlk;

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
/// Where an executable's segments may go: below the stack page, so they share one level-3 table with it.
const IMAGE: Range<u64> = USER_BASE..USER_STACK_TOP - PAGE as u64;
/// The boot archive (cpio, newc), built by `build.rs` from `crates/user`.
static ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/boot.cpio"));
/// EL1 virtual timer PPI.
const TIMER_IRQ: u32 = 27;
/// PL011 SPI 1 on QEMU `virt`.
const UART_IRQ: u32 = 33;
const TICK_US: u64 = 10_000;

/// GIC CPU interface base, set before the first IRQ can be delivered.
static GIC_CPU: AtomicU64 = AtomicU64::new(0);
/// Set once `Board::disk` handed out the block device.
static DISK_TAKEN: AtomicBool = AtomicBool::new(false);
/// QEMU `virt` has 32 virtio-mmio transports.
const MAX_VIRTIO: usize = 32;
/// Boot context included; a task's slot is its ASID (8 bits).
const MAX_TASKS: usize = 8;
const _: () = assert!(MAX_TASKS <= 256);
/// Kernel stack per task: 16 KiB.
const TASK_STACK_FRAMES: usize = 4;
const MAX_PIPES: usize = 16;
/// Each live mutex has a handle, so the handle tables are the per-process quota.
const MAX_MUTEXES: usize = MAX_TASKS * MAX_HANDLES;

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

/// Task contexts, free frames, pipes, mutexes and console input; touched only with IRQs masked on the only core.
static KERNEL: Global = Global(UnsafeCell::new(Kernel {
    sched: Scheduler::new(),
    frames: FrameAllocator::empty(),
    pipes: Pipes::new(),
    mutexes: Mutexes::new(),
    line: Line::new(),
}));

struct Kernel {
    sched: Scheduler<MAX_TASKS>,
    frames: FrameAllocator<FRAME_WORDS>,
    pipes: Pipes<MAX_PIPES>,
    mutexes: Mutexes<MAX_MUTEXES>,
    line: Line,
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
    // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
    unsafe { switch(sched, frame) }
}

/// Saves the current task's `frame` and enters the next ready one; returns its frame.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current task's trap frame.
unsafe fn switch(sched: &mut Scheduler<MAX_TASKS>, frame: usize) -> usize {
    let (_, from) = sched.current();
    let next = sched.switch(frame);
    if sched.current().1 != from {
        // SAFETY: `frame` came from the trap path and `next` from the scheduler.
        unsafe { enter(sched, frame, next) };
    }
    next
}

/// Ends the current process with `code`, returns all its frames, and returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), the current task must be a process, and `frame` its trap frame.
unsafe fn task_exit(frame: usize, code: u64) -> usize {
    // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    } = unsafe { &mut *KERNEL.0.get() };
    // Before `exit` picks the next task, so a reader or locker this wakes can be it.
    for index in mutexes.release(sched.current().0) {
        sched.wake(Event::Lock(index));
    }
    for object in core::mem::take(sched.handles()).objects() {
        release(sched, frames, pipes, mutexes, object);
    }
    let (asid, l1) = sched.current();
    let (next, stack) = sched.exit(code);
    // SAFETY: `frame` is the exiting process's trap frame and `next` came from the scheduler.
    unsafe { enter(sched, frame, next) };
    arch::flush_asid(asid);
    // Frees the kernel stack this runs on: sound only while nothing allocates before the trap returns to `next`.
    // SAFETY: TTBR0 left `l1` above, and its tables hold only this process's frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    free_stack(frames, stack);
    next
}

/// Blocks the current process on `event` with its `svc` rewound, so the call runs again once woken; returns the next
/// task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current process's.
unsafe fn block(
    sched: &mut Scheduler<MAX_TASKS>,
    frame: &mut arch::TrapFrame,
    event: Event,
) -> usize {
    frame.restart();
    sched.block(event);
    // SAFETY: the caller masked IRQs, and `frame` is the current process's.
    unsafe { switch(sched, frame as *mut arch::TrapFrame as usize) }
}

/// Drops one handle to `object`: an exited process frees its slot and, as `wait` does, moves its budget to the
/// current task; a pipe wakes its waiters and, once no handle reaches it, frees its page, refunding its creator if that
/// still runs; the last handle to a mutex frees it.
fn release(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    object: Object,
) {
    let end = match object {
        Object::Pipe(end) => end,
        Object::Mutex(mutex) => return mutexes.close(mutex),
        Object::Process { slot, generation } => {
            let limit = sched.close(slot, generation);
            let held = pipes.charged_to((slot, generation));
            return sched.memory().budget.grow(limit.saturating_sub(held));
        }
        _ => return,
    };
    if let Some((page, (slot, generation))) = pipes.close(end) {
        match sched.budget(slot, generation) {
            Some(budget) => budget.free(frames, page),
            None => frames.free(page),
        }
    }
    sched.wake(Event::Pipe(end.index as usize));
}

/// Creates a pipe whose page is charged to the current process, which gets a handle to each end (read, write).
fn new_pipe(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
) -> Result<(u64, u64), i64> {
    let read = pipes.free().ok_or(ENFILE)?;
    let write = End {
        write: true,
        ..read
    };
    let mut handles = *sched.handles();
    let read_handle = handles.insert(Object::Pipe(read), READ | DUPLICATE | TRANSFER)?;
    let write_handle = handles.insert(Object::Pipe(write), WRITE | DUPLICATE | TRANSFER)?;
    let page = sched.memory().budget.alloc(frames).ok_or(ENOMEM)?;
    pipes.create(read, page, (sched.current().0, sched.generation()));
    *sched.handles() = handles;
    Ok((read_handle, write_handle))
}

/// Moves bytes between the user buffer at `ptr` and the pipe `end` reaches; `None` if the caller must wait.
fn pipe_io(pipes: &mut Pipes<MAX_PIPES>, end: End, ptr: u64, len: usize) -> Option<i64> {
    let Some(pipe) = pipes.get(end) else {
        return Some(EBADF);
    };
    // SAFETY: an open pipe's page is identity-mapped RAM that only it uses, never mapped to user space.
    let page = unsafe { &mut *(pipe.page.0 as *mut [u8; pipe::SIZE]) };
    match end.write {
        true => user_bytes(ptr, len).map_or(Some(EFAULT), |data| pipe.write(page, data)),
        false => user_bytes_mut(ptr, len).map_or(Some(EFAULT), |out| pipe.read(page, out)),
    }
}

/// Ends the process in `slot` with `generation`, not the current one, as a fault would, and returns all its frames;
/// 0, or `EBADF` once a newer task took the slot.
fn kill(
    Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    }: &mut Kernel,
    slot: usize,
    generation: u64,
) -> i64 {
    let (handles, l1, stack, blocked) = match sched.kill(slot, generation) {
        Ok(Some(ended)) => ended,
        Ok(None) => return 0,
        Err(error) => return error,
    };
    let owner = match blocked {
        Some(Event::Lock(index)) => mutexes.owner(index),
        _ => None,
    };
    for index in mutexes.release(slot) {
        sched.wake(Event::Lock(index));
    }
    for object in handles.objects() {
        release(sched, frames, pipes, mutexes, object);
    }
    if let Some(owner) = owner {
        sched.unboost(
            owner,
            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(owner)),
        );
    }
    arch::flush_asid(slot);
    // SAFETY: the process is not current, so TTBR0 is not `l1`, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    free_stack(frames, stack);
    0
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

/// Builds a process from `file`'s `segments` (entered at `entry`): address space, pages and kernel stack, all charged
/// to `budget`, and queues it in the free `slot` with `handles` at `priority`; on failure (`ENOMEM`) returns every
/// frame it took.
fn spawn_process(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    (file, segments, entry): (&[u8], impl Iterator<Item = Segment>, u64),
    mut budget: Budget,
    (slot, handles, priority): ((usize, u64), Handles, u8),
) -> Result<(), i64> {
    let l1 = zeroed(frames, &mut budget).ok_or(ENOMEM)?;
    // SAFETY: `l1` is a fresh, zeroed frame.
    unsafe { (l1.0 as *mut [u64; 2]).write(KERNEL_L1) };
    let stack = (|| {
        for segment in segments {
            let data = &file[segment.data];
            // No read-only non-executable access kind yet, so a read-only segment (flags R) maps executable.
            let access = match segment.writable {
                true => UserAccess::ReadWrite,
                false => UserAccess::ReadExecute,
            };
            for offset in (0..segment.size as usize).step_by(PAGE) {
                let va = segment.vaddr + offset as u64;
                let page = map_zeroed(frames, &mut budget, l1, va, access)?;
                let bytes = data.get(offset..).unwrap_or_default();
                let bytes = &bytes[..bytes.len().min(PAGE)];
                // SAFETY: `bytes` fits in the fresh frame `page`.
                unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), page.0 as *mut u8, bytes.len()) };
                // SAFETY: `page` is identity-mapped RAM.
                unsafe { arch::sync_icache(page.0 as usize, PAGE) };
            }
        }
        let stack_page = USER_STACK_TOP - PAGE as u64;
        map_zeroed(frames, &mut budget, l1, stack_page, UserAccess::ReadWrite)?;
        budget.alloc_contiguous(frames, TASK_STACK_FRAMES)
    })();
    let Some(stack) = stack else {
        // SAFETY: no TTBR0 ever used `l1`, and its tables hold only frames taken above.
        unsafe { arch::free_space(l1, |f| frames.free(f)) };
        return Err(ENOMEM);
    };
    // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new process.
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, USER_STACK_TOP) };
    let memory = Memory {
        stack: stack.start,
        budget,
        next: MAP_BASE,
    };
    sched.add(slot, frame, l1, memory, handles, priority);
    Ok(())
}

/// The boot archive's executable at `file` (byte offsets), checked, as `spawn_process` takes it.
fn executable(
    file: Range<usize>,
) -> Result<(&'static [u8], impl Iterator<Item = Segment>, u64), i64> {
    let file = &ARCHIVE[file];
    let elf = Elf::parse(file, IMAGE).ok_or(ENOEXEC)?;
    let entry = elf.entry;
    Ok((file, elf.segments(), entry))
}

/// Queues `executable` from boot context with init's handles, a budget of `budget` frames and `priority`.
fn spawn_init(
    executable: (&[u8], impl Iterator<Item = Segment>, u64),
    budget: usize,
    priority: u8,
) -> Result<(), i64> {
    let irq = arch::irq::disable();
    // SAFETY: IRQs are masked on the only core, so this is the sole reference.
    let Kernel { sched, frames, .. } = unsafe { &mut *KERNEL.0.get() };
    let added = sched.free_slot().ok_or(EAGAIN).and_then(|slot| {
        let init = (slot, Handles::init(slot.0, slot.1), priority);
        spawn_process(sched, frames, executable, Budget::new(budget), init)
    });
    arch::irq::restore(irq);
    added
}

/// Spawns the boot archive's executable at `file` with the `len` handles at user address `ptr` and `budget` frames
/// moved from the current process, which gets a handle to the child, at `priority` capped at the current process's
/// own; on failure nothing moves.
fn spawn(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    file: Range<usize>,
    (ptr, len): (u64, usize),
    budget: usize,
    priority: u64,
) -> Result<u64, i64> {
    let executable = executable(file)?;
    let bytes = user_bytes(ptr, len * 8).ok_or(EFAULT)?;
    let mut list = [0; MAX_HANDLES];
    for (handle, bytes) in list.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *handle = u64::from_le_bytes(*bytes);
    }
    let (mut parent, child) = sched.handles().split(&list[..len])?;
    if budget > sched.memory().budget.remaining() {
        return Err(ENOMEM);
    }
    let (slot, generation) = sched.free_slot().ok_or(EAGAIN)?;
    let process = parent.insert(Object::Process { slot, generation }, WAIT | KILL)?;
    let priority = priority.min(sched.priority().into()) as u8;
    let child = ((slot, generation), child, priority);
    spawn_process(sched, frames, executable, Budget::new(budget), child)?;
    sched.memory().budget.shrink(budget);
    *sched.handles() = parent;
    Ok(process)
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

/// Whether `allowed` holds for each page of the `len` bytes at `ptr`.
fn user_pages(ptr: u64, len: usize, allowed: fn(u64) -> bool) -> bool {
    let first_page = ptr & !(PAGE as u64 - 1);
    (first_page..ptr + len as u64).step_by(PAGE).all(allowed)
}

/// The `len` bytes at user address `ptr` (in user space unless `len` is 0, checked by `dispatch`) if EL0 may read all
/// of them; valid only until the trap returns.
fn user_bytes<'a>(ptr: u64, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if !user_pages(ptr, len, arch::user_readable) {
        return None;
    }
    // SAFETY: EL0 may read every page of the range, so it is mapped in the current address space, which
    // stays loaded and unchanged until the trap returns (IRQs masked, one core).
    Some(unsafe { slice::from_raw_parts(ptr as *const u8, len) })
}

/// As `user_bytes`, if EL0 may write all of them.
fn user_bytes_mut<'a>(ptr: u64, len: usize) -> Option<&'a mut [u8]> {
    if len == 0 {
        return Some(&mut []);
    }
    if !user_pages(ptr, len, arch::user_writable) {
        return None;
    }
    // SAFETY: as in `user_bytes`; the pages are user memory, which no kernel reference aliases.
    Some(unsafe { slice::from_raw_parts_mut(ptr as *mut u8, len) })
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
    type Disk = VirtioBlk;

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
        // SAFETY: the DTB's GICv2 distributor, in the device-mapped GiB 0.
        unsafe { arch::gic::unmask(self.gic.0, TIMER_IRQ) };
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
        let Kernel { sched, frames, .. } = unsafe { &mut *KERNEL.0.get() };
        let added = sched.free_slot().ok_or(Full).and_then(|slot| {
            let stack = frames.alloc_contiguous(TASK_STACK_FRAMES).ok_or(Full)?;
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
            sched.add(slot, frame, PhysAddr(0), memory, Handles::new(), 0);
            Ok(())
        });
        arch::irq::restore(irq);
        added
    }

    fn yield_now(&mut self) {
        arch::yield_now()
    }

    fn run_others(&mut self) {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        unsafe { &mut (*KERNEL.0.get()).sched }.block(Event::Idle);
        arch::irq::restore(irq);
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

    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), i64> {
        let (code, entry) = user_program(program);
        let segment = Segment {
            vaddr: entry,
            data: 0..code.len(),
            size: code.len() as u64,
            writable: false,
        };
        spawn_init((code, [segment].into_iter(), entry), budget, 0)
    }

    fn spawn_archived(&mut self, name: &str, budget: usize) -> Result<(), i64> {
        let file = kernel::cpio::find(ARCHIVE, name.as_bytes()).ok_or(ENOENT)?;
        spawn_init(executable(file)?, budget, PRIORITIES - 1)
    }

    fn tasks(&self) -> usize {
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let count = unsafe { &(*KERNEL.0.get()).sched }.count();
        arch::irq::restore(irq);
        count
    }

    fn disk(&mut self, dtb: &Dtb) -> Option<VirtioBlk> {
        let alloc = || {
            let irq = arch::irq::disable();
            // SAFETY: IRQs are masked on the only core, so this is the sole reference.
            let frame = unsafe { &mut (*KERNEL.0.get()).frames }.alloc();
            arch::irq::restore(irq);
            frame
        };
        if DISK_TAKEN.swap(true, Relaxed) {
            return None;
        }
        let (mut bases, mut count) = ([PhysAddr(0); MAX_VIRTIO], 0);
        dtb.virtio_mmio(|base| {
            *bases.get_mut(count)? = base;
            count += 1;
            None::<()>
        });
        // QEMU `virt` fills the transports from the highest address down with no gaps, so the first empty one ends them.
        for &base in bases[..count].iter().rev() {
            // SAFETY: the DTB's virtio-mmio transports, in the device-mapped GiB 0, driven only here (`DISK_TAKEN`);
            // frames from the allocator are identity-mapped RAM nobody else uses.
            match unsafe { VirtioBlk::new(base, alloc) } {
                Ok(disk) => return Some(disk),
                Err(0) => break,
                Err(_) => {}
            }
        }
        None
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
    GIC_CPU.store(gic.1.0, Relaxed);
    // SAFETY: the DTB's GICv2 registers, in the device-mapped GiB 0.
    unsafe { arch::gic::enable(gic.0, gic.1) };
    // SAFETY: as above.
    unsafe { arch::gic::unmask(gic.0, UART_IRQ) };
    Uart::new(UART0).enable_rx_irq();

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
    // SAFETY: IRQs are delivered only after `kmain` stored the DTB's GIC CPU interface.
    let iar = unsafe { arch::gic::ack(cpu) };
    let tick = iar == TIMER_IRQ;
    if tick {
        arch::timer::arm(TICK_US);
    } else if iar == UART_IRQ {
        // SAFETY: the caller masked IRQs on the only core, so this is the sole reference.
        let Kernel { sched, line, .. } = unsafe { &mut *KERNEL.0.get() };
        let mut uart = Uart::new(UART0);
        while let Some(byte) = uart.get() {
            if line.push(byte, |echo| uart.write(echo)) {
                sched.wake(Event::Console);
            }
        }
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
    let kernel = unsafe { &mut *KERNEL.0.get() };
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        line,
    } = kernel;
    let args = frame.x.first_chunk().unwrap();
    frame.x[0] = match kernel::syscall::dispatch(frame.x[8], args, sched.handles()) {
        Ok(Call::Exit(code)) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { task_exit(frame as *mut arch::TrapFrame as usize, code) };
        }
        Ok(Call::Write { ptr, len }) => match user_bytes(ptr, len) {
            Some(bytes) => {
                Uart::new(UART0).write(bytes);
                len as u64
            }
            None => EFAULT as u64,
        },
        Ok(Call::Read { ptr, len }) => match user_bytes_mut(ptr, len).map(|out| line.read(out)) {
            None => EFAULT as u64,
            Some(Some(n)) => n as u64,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Some(None) => return unsafe { block(sched, frame, Event::Console) },
        },
        Ok(Call::Pipe { end, ptr, len }) => match pipe_io(pipes, end, ptr, len) {
            Some(moved) => {
                if moved > 0 {
                    sched.wake(Event::Pipe(end.index as usize));
                }
                moved as u64
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            None => return unsafe { block(sched, frame, Event::Pipe(end.index as usize)) },
        },
        Ok(Call::NewPipe) => match new_pipe(sched, frames, pipes) {
            Ok((read, write)) => {
                frame.x[1] = write;
                read
            }
            Err(error) => error as u64,
        },
        Ok(Call::Wait { slot, generation }) => match sched.reap(slot, generation) {
            Ok(Some((code, limit))) => {
                // Pipes it created that are still open keep their page; a repeated `wait` gets a limit of 0.
                let held = pipes.charged_to((slot, generation));
                sched.memory().budget.grow(limit.saturating_sub(held));
                code
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(sched, frame, Event::Exit(slot)) },
            Err(error) => error as u64,
        },
        Ok(Call::Dup { handle, object }) => {
            match object {
                Object::Pipe(end) => pipes.open(end),
                Object::Mutex(mutex) => mutexes.open(mutex),
                _ => {}
            }
            handle
        }
        Ok(Call::Close(object)) => {
            release(sched, frames, pipes, mutexes, object);
            0
        }
        Ok(Call::Map { pages }) => map(sched, frames, pages).unwrap_or(ENOMEM as u64),
        Ok(Call::Open { ptr, len, rights }) => user_bytes(ptr, len)
            .ok_or(EFAULT)
            .and_then(|name| kernel::cpio::find(ARCHIVE, name).ok_or(ENOENT))
            .and_then(|file| {
                let (start, end) = (file.start, file.end);
                sched.handles().insert(Object::File { start, end }, rights)
            })
            .unwrap_or_else(|error| error as u64),
        Ok(Call::Spawn {
            file,
            ptr,
            len,
            budget,
            priority,
        }) => spawn(sched, frames, file, (ptr, len), budget, priority)
            .unwrap_or_else(|error| error as u64),
        Ok(Call::NewMutex) => match mutexes.create() {
            Some(mutex) => sched
                .handles()
                .insert(Object::Mutex(mutex), DUPLICATE | TRANSFER)
                .unwrap_or_else(|error| {
                    mutexes.close(mutex);
                    error as u64
                }),
            None => ENFILE as u64,
        },
        Ok(Call::Lock(mutex)) => match mutexes.lock(mutex, sched.current().0) {
            Ok(None) => 0,
            Ok(Some(owner)) => {
                sched.boost(owner);
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { block(sched, frame, Event::Lock(mutex.index as usize)) };
            }
            Err(error) => error as u64,
        },
        Ok(Call::Unlock(mutex)) => {
            let slot = sched.current().0;
            match mutexes.unlock(mutex, slot) {
                Ok(()) => {
                    // With no waiter woken, the caller's boost is unchanged and nothing new is ready.
                    if sched.wake(Event::Lock(mutex.index as usize)) {
                        sched.unboost(
                            slot,
                            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(slot)),
                        );
                        if sched.outranked() {
                            frame.x[0] = 0;
                            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                            return unsafe {
                                switch(sched, frame as *mut arch::TrapFrame as usize)
                            };
                        }
                    }
                    0
                }
                Err(error) => error as u64,
            }
        }
        Ok(Call::Kill { slot, generation })
            if (slot, generation) == (sched.current().0, sched.generation()) =>
        {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { task_exit(frame as *mut arch::TrapFrame as usize, KILLED) };
        }
        Ok(Call::Kill { slot, generation }) => kill(kernel, slot, generation) as u64,
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
    unsafe { task_exit(frame, KILLED) }
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
