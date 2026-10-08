//! A `test=lock-split` worker: spins until the counter reaches its argument (microseconds), then makes `WRITES` 0-byte
//! console writes and a one-page `map` and prints `W: done`; none of it needs the kernel's big lock, which the boot
//! context holds from just before that time on. Exits 1 on any failure.
#![no_std]
#![no_main]

use user::*;

const WRITES: u64 = 10_000;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let start = arg(args, 1)
        .iter()
        .fold(0, |n, &d| n * 10 + (d - b'0') as u64);
    while now_ns() / 1000 < start {
        core::hint::spin_loop();
    }
    let ok = (0..WRITES).all(|_| write(CONSOLE, &[]) == 0) && map(4096).is_some();
    if !ok {
        return 1;
    }
    write(CONSOLE, b"W: done\n");
    0
}
