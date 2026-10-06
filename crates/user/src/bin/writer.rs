//! Spawned by `reader` with the console (handle 0) and a pipe's write end (handle 1).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    write(CONSOLE, b"W: writing to the pipe\n");
    write(1, b"hello\n");
    write(CONSOLE, b"W: exiting with 7\n");
    exit(7)
}
