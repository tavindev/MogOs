#![no_std]

extern crate alloc;

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
    /// Raises a breakpoint and reports whether it was caught and resumed.
    fn breakpoint_self_test(&mut self) -> bool;
    /// Identity-maps device memory and RAM and turns on the MMU and caches.
    fn enable_mmu(&mut self);
    /// Reads an address the MMU leaves unmapped; the data abort panics.
    fn read_unmapped(&mut self);
    /// Hands `region` (identity-mapped RAM, owned by nobody else) to the global allocator.
    fn init_heap(&mut self, region: Range<PhysAddr>);
    /// Microseconds since the board entered the kernel.
    fn uptime_us(&self) -> u64;
    fn power_off(&mut self) -> !;
}

/// Bitmap capacity in 64-frame words: 512 words cover 128 MiB.
const FRAME_WORDS: usize = 512;
/// 1 MiB kernel heap.
const HEAP_FRAMES: usize = 256;

/// `dtb` is the device tree blob; `reserved` lists physical ranges in use (kernel image, DTB).
pub fn run<B: Board>(board: &mut B, dtb: &[u8], reserved: &[Range<PhysAddr>]) -> ! {
    let el = board.exception_level();
    let _ = writeln!(board.console(), "MogOs: hello from EL{el}");

    let ok = board.breakpoint_self_test();
    let _ = writeln!(
        board.console(),
        "exceptions: {}",
        if ok { "ok" } else { "FAILED" }
    );

    let dtb = Dtb::new(dtb).expect("bad DTB");
    let ram = dtb.memory().expect("no memory node in DTB");
    let _ = writeln!(board.console(), "ram: {:#x}..{:#x}", ram.start, ram.end);

    let mut frames = FrameAllocator::<FRAME_WORDS>::new(ram);
    for range in reserved {
        frames.reserve(range.clone());
    }
    let _ = writeln!(board.console(), "frames: {} free", frames.free_count());

    board.enable_mmu();
    let _ = writeln!(board.console(), "mmu: on");
    let bootargs = dtb.bootargs().unwrap_or_default();
    if bootargs.split_whitespace().any(|a| a == "test=mmu-fault") {
        board.read_unmapped();
    }

    let heap = frames
        .alloc_contiguous(HEAP_FRAMES)
        .expect("no room for heap");
    board.init_heap(heap);
    let mut v = Vec::new();
    for i in 1..=1000u64 {
        v.push(i);
    }
    let ok = v.iter().sum::<u64>() == 500_500;
    drop(v);
    let _ = writeln!(
        board.console(),
        "heap: {}",
        if ok { "ok" } else { "FAILED" }
    );

    let boot_us = board.uptime_us();
    let _ = writeln!(board.console(), "boot: {boot_us} us");

    board.power_off()
}
