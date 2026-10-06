#![no_std]

extern crate alloc;

mod sched;

pub use sched::{Full, Scheduler};

use alloc::vec::Vec;
use core::fmt::Write;
use core::ops::Range;

use dtb::Dtb;
use mm::{FrameAllocator, PhysAddr};

/// What the kernel needs from the hardware; each board implements it.
pub trait Board {
    type Console: Write;

    fn console(&mut self) -> &mut Self::Console;
    fn exception_level(&self) -> u8;
    /// Raises a breakpoint; returns once it was caught and resumed.
    fn breakpoint_self_test(&mut self);
    /// Identity-maps device memory and RAM and turns on the MMU and caches.
    fn enable_mmu(&mut self);
    /// Reads an address the MMU leaves unmapped; the data abort panics.
    fn read_unmapped(&mut self);
    /// Hands `region` (identity-mapped RAM, owned by nobody else) to the global allocator; call once.
    fn init_heap(&mut self, region: Range<PhysAddr>);
    /// Microseconds since the board entered the kernel.
    fn uptime_us(&self) -> u64;
    /// Starts the periodic timer interrupt; IRQs stay masked outside `idle`.
    fn start_timer(&mut self);
    /// Timer interrupts handled since `start_timer`.
    fn ticks(&self) -> u64;
    /// Sleeps until an interrupt arrives and handles it.
    fn idle(&mut self);
    fn power_off(&mut self) -> !;
    /// Queues a task that runs `entry(board, arg)` on its own stack with its own board handle.
    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full>;
    /// Runs the other tasks in turn; returns when this one is scheduled again.
    fn yield_now(&mut self);
}

/// Round trips timed by `test=bench`.
const BENCH_YIELDS: u64 = 100_000;

/// Bitmap capacity in 64-frame words: 512 words cover 128 MiB.
const FRAME_WORDS: usize = 512;
/// 1 MiB kernel heap.
const HEAP_FRAMES: usize = 256;

/// `reserved` lists physical ranges in use (kernel image, DTB).
pub fn run<B: Board>(board: &mut B, dtb: Dtb, reserved: &[Range<PhysAddr>]) -> ! {
    let el = board.exception_level();
    let _ = writeln!(board.console(), "MogOs: hello from EL{el}");

    board.breakpoint_self_test();
    let _ = writeln!(board.console(), "exceptions: ok");

    board.enable_mmu();
    let _ = writeln!(board.console(), "mmu: on");
    let bootargs = dtb.bootargs().unwrap_or_default();
    if bootargs.split_whitespace().any(|a| a == "test=mmu-fault") {
        board.read_unmapped();
    }

    let ram = dtb.memory().expect("no memory node in DTB");
    let _ = writeln!(board.console(), "ram: {:#x}..{:#x}", ram.start, ram.end);

    let mut frames = FrameAllocator::<FRAME_WORDS>::new(ram);
    for range in reserved {
        frames.reserve(range.clone());
    }
    let _ = writeln!(board.console(), "frames: {} free", frames.free_count());

    let heap = frames
        .alloc_contiguous(HEAP_FRAMES)
        .expect("no room for heap");
    board.init_heap(heap);
    assert_eq!(
        (1..=1000u64).collect::<Vec<_>>().iter().sum::<u64>(),
        500_500
    );
    let _ = writeln!(board.console(), "heap: ok");

    let boot_us = board.uptime_us();
    let _ = writeln!(board.console(), "boot: {boot_us} us");

    for arg in bootargs.split_whitespace() {
        match arg {
            "test=yield" => yield_demo(board),
            "test=bench" => yield_bench(board),
            _ => {}
        }
    }

    board.start_timer();
    while board.ticks() < 3 {
        board.idle();
    }
    let ticks = board.ticks();
    let _ = writeln!(board.console(), "ticks: {ticks}");

    board.power_off()
}

/// Tasks a and b print in turn; the boot task's third yield returns after both printed 2.
fn yield_demo<B: Board>(board: &mut B) {
    board.spawn(print_and_yield, 'a' as usize).expect("spawn a");
    board.spawn(print_and_yield, 'b' as usize).expect("spawn b");
    for _ in 0..3 {
        board.yield_now();
    }
}

fn print_and_yield<B: Board>(board: &mut B, name: usize) -> ! {
    let name = name as u8 as char;
    let mut i = 0;
    loop {
        let _ = writeln!(board.console(), "task {name}: {i}");
        i += 1;
        board.yield_now();
    }
}

/// Each boot-task yield is one round trip through a task that only yields back.
fn yield_bench<B: Board>(board: &mut B) {
    board.spawn(yield_forever, 0).expect("spawn");
    let start = board.uptime_us();
    for _ in 0..BENCH_YIELDS {
        board.yield_now();
    }
    let ns = (board.uptime_us() - start) * 1000 / BENCH_YIELDS;
    let _ = writeln!(board.console(), "yield: {ns} ns/round-trip");
}

fn yield_forever<B: Board>(board: &mut B, _: usize) -> ! {
    loop {
        board.yield_now();
    }
}
