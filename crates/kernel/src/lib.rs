#![no_std]

extern crate alloc;

pub mod console;
pub mod cpio;
pub mod elf;
pub mod file;
pub mod handle;
pub mod mutex;
pub mod pipe;
mod sched;
pub mod syscall;

pub use mogfs::{BLOCK_SIZE, Disk};
pub use sched::{Event, Full, Memory, PRIORITIES, Scheduler};

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ops::Range;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::Relaxed;

use dtb::Dtb;
use mm::{FrameAllocator, PhysAddr};

/// What the kernel needs from the hardware; each board implements it.
pub trait Board {
    type Console: Write;
    type Disk: Disk;

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
    /// Queues a task that runs `entry(board, arg)` on its own stack with its own board handle, at priority 0.
    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full>;
    /// Runs the other tasks in turn; returns when this one is scheduled again.
    fn yield_now(&mut self);
    /// Runs the other tasks until none is ready (each exited or blocked). Boot context only.
    fn run_others(&mut self);
    /// Takes over the frame allocator: process memory and kernel stacks come from it from now on; call once.
    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>);
    fn free_frames(&self) -> usize;
    /// Queues `program` as a process at EL0 in its own address space, with a budget of `budget` frames that pays for
    /// its tables, pages and kernel stack, and init's handles (`Handles::init`), at priority 0 like the boot context;
    /// `ENOMEM` or `EAGAIN` (no free slot).
    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), i64>;
    /// As `spawn_user`, for the boot archive's executable `name`, at the top priority (`PRIORITIES - 1`); `ENOENT` or
    /// `ENOEXEC` if it is missing or invalid.
    fn spawn_archived(&mut self, name: &str, budget: usize) -> Result<(), i64>;
    /// Tasks in the run queue, the boot context included.
    fn tasks(&self) -> usize;
    /// The board's block device, set up with memory from the frame allocator; call once, after `init_frames`.
    fn disk(&mut self) -> Option<Self::Disk>;
    /// Mounts the MogFS on `disk` as the board's file system; once it is mounted, every process spawned from boot
    /// context also gets its root directory (read, write, duplicate, transfer) as handle 3. Never formats.
    fn mount(&mut self, disk: Self::Disk) -> Result<(), mogfs::Error>;
    /// Makes `n` uncontended acquire + release round trips on a ticket lock like the board's kernel lock (`ticket`),
    /// or on a test-and-set lock.
    fn lock_round_trips(&mut self, n: u64, ticket: bool);
    /// Adds 1 to a counter `n` times, taking a board `Lock` (the kind `KERNEL` is) for each; returns the counter.
    fn add_locked(&mut self, n: u64) -> u64;
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
/// Round trips per lock timed by `test=bench-lock`, and the additions each of its two adders makes.
const BENCH_LOCKS: u64 = 10_000_000;
/// `test=bench-lock`'s adders that are done.
static ADDERS_DONE: AtomicUsize = AtomicUsize::new(0);
/// Round trips the boot archive's `ping` makes with `pong` under `test=bench-pipe`; must equal its `ROUND_TRIPS`.
const PIPE_ROUND_TRIPS: u64 = 100_000;
/// Blocks `test=bench-disk` writes and reads (8 MiB); the disk must hold at least this many.
const DISK_BENCH_BLOCKS: u64 = 2048;
/// Blocks per request in `test=bench-disk`'s batched pass (256 KiB of heap).
const DISK_BATCH: usize = 64;

/// Bitmap capacity in 64-frame words: 512 words cover 128 MiB.
pub const FRAME_WORDS: usize = 512;
/// 1 MiB kernel heap.
const HEAP_FRAMES: usize = 256;
/// Each boot-spawned process's budget in frames; a process moves part of its own to each child it spawns.
const BOOT_BUDGET: usize = 25;
/// `waiter`'s 9 frames and its two children's 9 and 10 at once.
const WAITER_BUDGET: usize = 28;
/// `pi`'s 9 frames, its two pipes' pages and its three children's 9 each.
const PI_BUDGET: usize = 38;

/// `reserved` lists physical ranges in use (kernel image, DTB).
pub fn run<B: Board>(board: &mut B, dtb: Dtb, reserved: &[Range<PhysAddr>]) -> ! {
    // First: console writes and traps take locks, whose atomics need the MMU on.
    board.enable_mmu();
    let el = board.exception_level();
    let _ = writeln!(board.console(), "MogOs: hello from EL{el}");

    board.breakpoint_self_test();
    let _ = writeln!(board.console(), "exceptions: ok");

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

    let mut disk = board.disk();
    let blocks = disk.as_ref().map(Disk::blocks);
    // The raw disk tests keep the device to themselves.
    let raw = bootargs
        .split_whitespace()
        .any(|a| a == "test=disk" || a == "test=bench-disk");
    let mounted = disk.take_if(|_| !raw).map(|disk| board.mount(disk));

    let boot_us = board.uptime_us();
    let _ = writeln!(board.console(), "boot: {boot_us} us");
    match blocks {
        Some(blocks) => writeln!(board.console(), "disk: {blocks} blocks"),
        None => writeln!(board.console(), "disk: none"),
    }
    .ok();
    if let Some(Err(error)) = mounted {
        let _ = writeln!(board.console(), "fs: {error:?}");
    }

    for arg in bootargs.split_whitespace() {
        match arg {
            "test=yield" => yield_demo(board),
            "test=bench" => yield_bench(board),
            "test=preempt" => preempt_demo(board),
            "test=user" => user_demo(board),
            "test=bench-syscall" => run_alone(board, Program::SyscallBench),
            "test=handles" => run_alone(board, Program::Handles),
            "test=spawn" => run_archived(board, "spawn", "spawner", BOOT_BUDGET),
            "test=pipe" => run_archived(board, "pipe", "reader", BOOT_BUDGET),
            "test=wait" => run_archived(board, "wait", "waiter", WAITER_BUDGET),
            "test=echo" => run_archived(board, "echo", "readlines", BOOT_BUDGET),
            "test=shell" => run_archived(board, "shell", "msh", BOOT_BUDGET),
            "test=bench-fs" => run_archived(board, "bench-fs", "fsbench", BOOT_BUDGET),
            "test=bench-spawn" => run_archived(board, "bench-spawn", "spawnbench", BOOT_BUDGET),
            "test=pi" => {
                board.start_timer();
                run_archived(board, "pi", "pi", PI_BUDGET);
            }
            "test=bench-pipe" => pipe_bench(board),
            "test=bench-lock" => lock_bench(board),
            "test=budget" => {
                let before = board.free_frames();
                run_alone(board, Program::Budget);
                let after = board.free_frames();
                let _ = writeln!(
                    board.console(),
                    "budget: free frames {before} before, {after} after"
                );
            }
            "test=disk" => disk_test(board, disk.as_mut().expect("no disk")),
            "test=bench-disk" => disk_bench(board, disk.as_mut().expect("no disk")),
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

/// Runs the boot archive's `program` with `budget` frames until every task has exited; prints the free frames before
/// and after as `<test>: free frames <n> before, <n> after`.
fn run_archived<B: Board>(board: &mut B, test: &str, program: &str, budget: usize) {
    let before = board.free_frames();
    board.spawn_archived(program, budget).expect("spawn");
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

/// Times uncontended acquire + release of the ticket and the test-and-set lock; then two tasks, preempted by the timer,
/// each add `BENCH_LOCKS` to a counter under the board's lock, and the total must be exact.
fn lock_bench<B: Board>(board: &mut B) {
    for (name, ticket) in [("ticket", true), ("test-and-set", false)] {
        let start = board.uptime_us();
        board.lock_round_trips(BENCH_LOCKS, ticket);
        let tenths = (board.uptime_us() - start) * 10_000 / BENCH_LOCKS;
        let (ns, tenth) = (tenths / 10, tenths % 10);
        let _ = writeln!(board.console(), "lock: {name} {ns}.{tenth} ns/round-trip");
    }
    board.spawn(add_and_yield, 0).expect("spawn");
    board.spawn(add_and_yield, 0).expect("spawn");
    board.start_timer();
    while ADDERS_DONE.load(Relaxed) < 2 {
        board.idle();
    }
    let count = board.add_locked(0);
    let _ = writeln!(board.console(), "lock: count {count}");
}

fn add_and_yield<B: Board>(board: &mut B, _: usize) -> ! {
    let count = board.add_locked(BENCH_LOCKS);
    let _ = writeln!(board.console(), "lock: adder done at {count}");
    ADDERS_DONE.fetch_add(1, Relaxed);
    loop {
        board.yield_now();
    }
}

fn yield_forever<B: Board>(board: &mut B, _: usize) -> ! {
    loop {
        board.yield_now();
    }
}

/// Reads blocks 1 and 2 in one request and prints `disk: read ok` if they hold the test pattern (byte `i` of the two
/// is `i % 251`); otherwise writes it in one request, flushes and prints `disk: wrote` (`disk: flush failed` if the
/// flush fails). So the first boot on a zeroed image writes, and the next one reads it back. Empty reads and writes
/// must succeed, and a read straddling the last block must be `Io`.
fn disk_test<B: Board>(board: &mut B, disk: &mut B::Disk) {
    let pattern: [[u8; BLOCK_SIZE]; 2] =
        core::array::from_fn(|b| core::array::from_fn(|i| ((b * BLOCK_SIZE + i) % 251) as u8));
    let mut block = [[0; BLOCK_SIZE]; 2];
    disk.read(0, &mut []).expect("empty read");
    disk.write(0, &[]).expect("empty write");
    assert_eq!(
        disk.read(disk.blocks() - 1, &mut block),
        Err(mogfs::Error::Io)
    );
    disk.read(1, &mut block).expect("read");
    let done = if block == pattern {
        "read ok"
    } else {
        disk.write(1, &pattern).expect("write");
        match disk.flush() {
            Ok(()) => "wrote",
            Err(_) => "flush failed",
        }
    };
    let _ = writeln!(board.console(), "disk: {done}");
}

/// Writes `DISK_BENCH_BLOCKS` blocks in order and flushes, then reads them back, one block and then `DISK_BATCH`
/// blocks per request; prints each throughput in MiB/s.
fn disk_bench<B: Board>(board: &mut B, disk: &mut B::Disk) {
    let mut blocks = vec![[0x5a; BLOCK_SIZE]; DISK_BATCH];
    let mib_s = |us: u64| ((DISK_BENCH_BLOCKS * BLOCK_SIZE as u64) >> 20) * 1_000_000 / us;
    for batch in [1, DISK_BATCH] {
        let kib = batch * BLOCK_SIZE / 1024;
        let start = board.uptime_us();
        for n in (0..DISK_BENCH_BLOCKS).step_by(batch) {
            disk.write(n, &blocks[..batch]).expect("write");
        }
        disk.flush().expect("flush");
        let write = mib_s(board.uptime_us() - start);
        let start = board.uptime_us();
        for n in (0..DISK_BENCH_BLOCKS).step_by(batch) {
            disk.read(n, &mut blocks[..batch]).expect("read");
        }
        let read = mib_s(board.uptime_us() - start);
        let _ = writeln!(board.console(), "disk: {kib} KiB write+flush {write} MiB/s");
        let _ = writeln!(board.console(), "disk: {kib} KiB read {read} MiB/s");
    }
}
