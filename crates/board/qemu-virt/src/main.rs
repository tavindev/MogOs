#![no_std]
#![no_main]

extern crate alloc;

mod uart;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use arch::{MemoryType, l1_block};
use dtb::Dtb;
use kernel::{Full, Scheduler};
use linked_list_allocator::Heap;
use mm::PhysAddr;
use uart::Uart;

/// Panic-path console; normal output uses the PL011 from the DTB.
const UART0: PhysAddr = PhysAddr(0x0900_0000);
/// QEMU loads the DTB at RAM base for an ELF kernel (x0 stays 0), if it fits below the image.
const DTB: PhysAddr = PhysAddr(0x4000_0000);
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const GIB: u64 = 1 << 30;
/// Outside both mapped GiBs.
const UNMAPPED: PhysAddr = PhysAddr(0x8000_0000);
/// EL1 virtual timer PPI.
const TIMER_IRQ: u32 = 27;
const TICK_US: u64 = 10_000;

/// GIC CPU interface base, set before the first IRQ is unmasked.
static GIC_CPU: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Boot context included.
const MAX_TASKS: usize = 8;
/// 16 KiB, 16-byte aligned.
const TASK_STACK: Layout = Layout::new::<[u128; 1024]>();

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

/// Task contexts; touched only with IRQs masked on the only core.
static SCHED: Sched = Sched(UnsafeCell::new(Scheduler::new()));

struct Sched(UnsafeCell<Scheduler<MAX_TASKS>>);

// SAFETY: one core, and the scheduler is only touched with IRQs masked, so accesses never overlap.
unsafe impl Sync for Sched {}

#[unsafe(no_mangle)]
extern "C" fn task_switch(frame: usize) -> usize {
    // SAFETY: called only from the `svc` trap handler, with IRQs masked on the only core, so this is the sole reference.
    unsafe { &mut *SCHED.0.get() }.switch(frame)
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
        let l1 = [
            l1_block(PhysAddr(0), MemoryType::Device),
            l1_block(PhysAddr(GIB), MemoryType::Normal),
        ];
        // SAFETY: called at boot with the MMU off, before any atomic RMW; MMIO is in GiB 0, and the image, stack and DTB are in RAM in GiB 1.
        unsafe { arch::enable_mmu(&l1, arch::MAIR) }
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

    fn ticks(&self) -> u64 {
        TICKS.load(Relaxed)
    }

    fn idle(&mut self) {
        arch::irq::wait()
    }

    fn power_off(&mut self) -> ! {
        shutdown()
    }

    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full> {
        // SAFETY: the layout has a nonzero size.
        let stack = unsafe { alloc::alloc::alloc(TASK_STACK) };
        if stack.is_null() {
            return Err(Full);
        }
        let start = (stack as usize + TASK_STACK.size() - size_of::<Start>()) & !15;
        let board = self.clone();
        // SAFETY: `start` is 16-byte aligned and inside the fresh stack, which nothing else references.
        unsafe { (start as *mut Start).write(Start { board, entry, arg }) };
        // SAFETY: `start` is 16-byte aligned and the stack below it is fresh and owned by the new task.
        let frame = unsafe { arch::new_task(start, task_start, start) };
        let irq = arch::irq::disable();
        // SAFETY: IRQs are masked on the only core, so this is the sole reference.
        let added = unsafe { &mut *SCHED.0.get() }.add(frame);
        arch::irq::restore(irq);
        if added.is_err() {
            // SAFETY: `stack` came from `alloc` with this layout and was never handed to the scheduler.
            unsafe { alloc::alloc::dealloc(stack, TASK_STACK) };
        }
        added
    }

    fn yield_now(&mut self) {
        arch::yield_now()
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

#[unsafe(no_mangle)]
extern "C" fn board_irq() {
    let cpu = PhysAddr(GIC_CPU.load(Relaxed));
    // SAFETY: IRQs are unmasked only after `start_timer` stored the DTB's GIC CPU interface.
    let iar = unsafe { arch::gic::ack(cpu) };
    if iar == TIMER_IRQ {
        arch::timer::arm(TICK_US);
        TICKS.fetch_add(1, Relaxed);
    }
    // SAFETY: as above.
    unsafe { arch::gic::eoi(cpu, iar) };
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
