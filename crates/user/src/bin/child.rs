//! Spawned by `spawner` with only the console, as handle 0.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use user::*;

/// In `.data` and `.bss`, so the loader copies one and zeroes the other in the RW segment.
static DATA: AtomicU64 = AtomicU64::new(1);
static BSS: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
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
    exit(0)
}
