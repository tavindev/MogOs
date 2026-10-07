//! `test=pi`'s H: spawned with the console and a mutex that L holds.
#![no_std]
#![no_main]

use user::*;

const MUTEX: u64 = 1;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    if unlock(MUTEX) == EPERM {
        write(CONSOLE, b"H: unlock while L owns it: EPERM\n");
    }
    write(CONSOLE, b"H: locking\n");
    if lock(MUTEX) == 0 {
        write(CONSOLE, b"H: acquired\n");
    }
    unlock(MUTEX);
    exit(0)
}
