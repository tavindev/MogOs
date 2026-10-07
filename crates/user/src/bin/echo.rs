//! Reads a line from the console and prints it back.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    write(CONSOLE, b"E: ready\n");
    let mut line = [0; 64];
    let n = read(CONSOLE, &mut line).max(0) as usize;
    write(CONSOLE, b"got: ");
    write(CONSOLE, &line[..n]);
    exit(0)
}
