//! Spawned by `ping` with a pipe's read end (handle 0) and another's write end (handle 1): echoes each byte until end
//! of file, then exits with 0 (or the failed read's code).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let mut byte = [0];
    loop {
        let n = read(0, &mut byte);
        if n <= 0 {
            exit(n as u64);
        }
        write(1, &byte);
    }
}
