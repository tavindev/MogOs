//! A `test=lock-split` worker: spawned with no budget to spare, spins on a one-page `map` until the boot context, holding
//! the kernel's big lock, raises its budget; then makes `WRITES` 0-byte console writes, prints `W: done` and maps 16
//! pages, which the boot context waits to see in its budget. None of it needs that lock. Exits 1 on any failure.
#![no_std]
#![no_main]

use user::*;

const WRITES: u64 = 10_000;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(_: &[&[u8]]) -> u64 {
    while map(4096).is_none() {
        core::hint::spin_loop();
    }
    if !(0..WRITES).all(|_| write(CONSOLE, &[]) == 0) {
        return 1;
    }
    write(CONSOLE, b"W: done\n");
    match map(16 * 4096) {
        Some(_) => 0,
        None => 1,
    }
}
