#![no_std]

extern crate alloc;

pub mod cpio;
pub mod elf;
pub mod handle;
pub mod pipe;
mod sched;
pub mod syscall;

pub use sched::{Event, Full, Memory, Scheduler};

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
    /// Starts the periodic timer interrupt; each tick switches to the next task. IRQs are unmasked only in tasks and `idle`.
    fn start_timer(&mut self);
    /// Sleeps until an interrupt arrives and handles it. Boot context only: returns with IRQs masked.
    fn idle(&mut self);
    fn power_off(&mut self) -> !;
    /// Queues a task that runs `entry(board, arg)` on its own stack with its own board handle.
    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full>;
    /// Runs the other tasks in turn; returns when this one is scheduled again.
    fn yield_now(&mut self);
    /// Runs the other tasks until none is ready (each exited or blocked). Boot context only.
    fn run_others(&mut self);
    /// Takes over the frame allocator: process memory and kernel stacks come from it from now on; call once.
    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>);
    fn free_frames(&self) -> usize;
    /// Queues `program` as a process at EL0 in its own address space, with a budget of `budget` frames that pays for
    /// its tables, pages and kernel stack, and init's handles (`Handles::init`); `ENOMEM` or `EAGAIN` (no free slot).
    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), i64>;
    /// As `spawn_user`, for the boot archive's executable `name`; `ENOENT` or `ENOEXEC` if it is missing or invalid.
    fn spawn_archived(&mut self, name: &str, budget: usize) -> Result<(), i64>;
    /// Tasks in the run queue, the boot context included.
    fn tasks(&self) -> usize;
}

/// Hand-written asm user programs the board provides; newer ones are ELF files in the boot archive.
pub enum Program {
    /// Checks that `write` rejects bad pointers, prints `A: 0`..`A: 9` with a spin after each, exits.
    Counter,
    /// Reads the counter's code address, which its own address space does not map.
    Intruder,
    /// Reads kernel RAM, which every address space maps for EL1 only; same address as `Intruder`.
    KernelReader,
    /// Times 100000 no-op syscalls (`io_submit_wait` writing 0 bytes) with the virtual counter, prints
    /// `syscall: <ns> ns/round-trip`.
    SyscallBench,
    /// Writes to the console, then through a duplicate without write, a closed handle and a stale one (its entry
    /// reused), printing a line for each expected result.
    Handles,
    /// Maps a page at a time, checking each is zeroed and writable, until `map` fails with `ENOMEM`; prints the
    /// page count, then a line showing it still runs.
    Budget,
}

/// Round trips timed by `test=bench`.
const BENCH_YIELDS: u64 = 100_000;
/// Round trips the boot archive's `ping` makes with `pong` under `test=bench-pipe`.
const PIPE_ROUND_TRIPS: u64 = 100_000;

/// Bitmap capacity in 64-frame words: 512 words cover 128 MiB.
pub const FRAME_WORDS: usize = 512;
/// 1 MiB kernel heap.
const HEAP_FRAMES: usize = 256;
/// Each boot-spawned process's budget in frames; a process moves part of its own to each child it spawns.
const BOOT_BUDGET: usize = 25;

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
    board.init_frames(frames);

    let boot_us = board.uptime_us();
    let _ = writeln!(board.console(), "boot: {boot_us} us");

    for arg in bootargs.split_whitespace() {
        match arg {
            "test=yield" => yield_demo(board),
            "test=bench" => yield_bench(board),
            "test=preempt" => preempt_demo(board),
            "test=user" => user_demo(board),
            "test=bench-syscall" => run_alone(board, Program::SyscallBench),
            "test=handles" => run_alone(board, Program::Handles),
            "test=spawn" => run_archived(board, "spawn", "spawner"),
            "test=pipe" => run_archived(board, "pipe", "reader"),
            "test=bench-pipe" => pipe_bench(board),
            "test=budget" => {
                let before = board.free_frames();
                run_alone(board, Program::Budget);
                let after = board.free_frames();
                let _ = writeln!(
                    board.console(),
                    "budget: free frames {before} before, {after} after"
                );
            }
            _ => {}
        }
    }

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

/// Task a never yields, so each line task b prints needs a tick to preempt a; b powers off after three.
fn preempt_demo<B: Board>(board: &mut B) -> ! {
    board.spawn(spin, 0).expect("spawn a");
    board.spawn(print_and_power_off, 0).expect("spawn b");
    board.start_timer();
    loop {
        board.idle();
    }
}

fn spin<B: Board>(_: &mut B, _: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

fn print_and_power_off<B: Board>(board: &mut B, _: usize) -> ! {
    for i in 0..3 {
        let _ = writeln!(board.console(), "task b: {i}");
        board.yield_now();
    }
    board.power_off()
}

/// The timer preempts process A between its lines; B faults on A's address and is killed; C then takes B's
/// slot (and ASID) and is killed for reading kernel memory; returns once all are gone.
fn user_demo<B: Board>(board: &mut B) {
    board
        .spawn_user(Program::Counter, BOOT_BUDGET)
        .expect("spawn A");
    board
        .spawn_user(Program::Intruder, BOOT_BUDGET)
        .expect("spawn B");
    board.start_timer();
    while board.tasks() > 2 {
        board.idle();
    }
    board
        .spawn_user(Program::KernelReader, BOOT_BUDGET)
        .expect("spawn C");
    while board.tasks() > 1 {
        board.idle();
    }
}

/// Runs the boot archive's `program` until every task has exited; prints the free frames before and after as
/// `<test>: free frames <n> before, <n> after`. The timer stays off.
fn run_archived<B: Board>(board: &mut B, test: &str, program: &str) {
    let before = board.free_frames();
    board.spawn_archived(program, BOOT_BUDGET).expect("spawn");
    wait(board);
    let after = board.free_frames();
    let _ = writeln!(
        board.console(),
        "{test}: free frames {before} before, {after} after"
    );
}

/// Runs `program` until it exits; the timer stays off so nothing preempts it.
fn run_alone<B: Board>(board: &mut B, program: Program) {
    board.spawn_user(program, BOOT_BUDGET).expect("spawn");
    wait(board);
}

/// Runs the other tasks until every one has exited; while all are blocked, sleeps until an interrupt.
fn wait<B: Board>(board: &mut B) {
    board.run_others();
    while board.tasks() > 1 {
        board.idle();
        board.run_others();
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

/// Times `ping`, which sends `pong` a byte and reads it back over two pipes, from its spawn until both exited; spawn
/// and exit are well under 1% of it.
fn pipe_bench<B: Board>(board: &mut B) {
    let start = board.uptime_us();
    board.spawn_archived("ping", BOOT_BUDGET).expect("spawn");
    wait(board);
    let ns = (board.uptime_us() - start) * 1000 / PIPE_ROUND_TRIPS;
    let _ = writeln!(board.console(), "pipe: {ns} ns/round-trip");
}

fn yield_forever<B: Board>(board: &mut B, _: usize) -> ! {
    loop {
        board.yield_now();
    }
}
