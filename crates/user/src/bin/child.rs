//! Spawned by `spawner` with only the console, as handle 0.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    write(CONSOLE, b"C: hello through handle 0\n");
    if write(1, b"C: handle 1 works\n") == EBADF {
        write(CONSOLE, b"C: handle 1 not given: EBADF\n");
    }
    exit(0)
}
