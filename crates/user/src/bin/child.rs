//! Spawned by `spawner` with only the console, as handle 0, and arguments, which it counts and prints the first two of.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use user::*;

/// In `.data` and `.bss`, so the loader copies one and zeroes the other in the RW segment.
static DATA: AtomicU64 = AtomicU64::new(1);
static BSS: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    write(CONSOLE, b"C: hello through handle 0\n");
    let initial = (DATA.load(Relaxed), BSS.load(Relaxed));
    DATA.store(2, Relaxed);
    BSS.store(3, Relaxed);
    if initial == (1, 0) && (DATA.load(Relaxed), BSS.load(Relaxed)) == (2, 3) {
        write(CONSOLE, b"C: statics work\n");
    }
    if write(1, b"C: handle 1 works\n") == EBADF {
        write(CONSOLE, b"C: handle 1 not given: EBADF\n");
    }
    if let [first, second, ..] = args {
        write(CONSOLE, b"C: ");
        write_u64(CONSOLE, args.len() as u64);
        write(CONSOLE, b" args of ");
        write_u64(CONSOLE, args.iter().map(|a| a.len() as u64 + 1).sum());
        write(CONSOLE, b" bytes: ");
        write(CONSOLE, first);
        write(CONSOLE, b", ");
        write(CONSOLE, second);
        write(CONSOLE, b"\n");
    }
    0
}
