//! `test=pi`'s Mid: spins forever, never blocking.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    write(CONSOLE, b"M: spinning\n");
    loop {
        core::hint::spin_loop()
    }
}
