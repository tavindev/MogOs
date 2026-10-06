//! `test=bench-pipe`'s init: sends `pong` one byte and reads it back, `ROUND_TRIPS` times, over two pipes; the kernel
//! times it from spawn to exit.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// Must equal the kernel's `PIPE_ROUND_TRIPS`, which it divides the time by.
const ROUND_TRIPS: usize = 100_000;
/// `pong`'s 9 frames (3 tables, text, stack, 4 kernel stack).
const PONG_BUDGET: usize = 9;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let (to_pong_read, to_pong) = pipe();
    let (from_pong, from_pong_write) = pipe();
    let exe = open(DIR, b"pong", EXEC) as u64;
    let pong = spawn(exe, &[to_pong_read as u64, from_pong_write], PONG_BUDGET);
    if to_pong_read < 0 || from_pong < 0 || pong < 0 {
        exit(1);
    }
    let mut byte = [0];
    for _ in 0..ROUND_TRIPS {
        write(to_pong, &byte);
        if read(from_pong as u64, &mut byte) != 1 {
            exit(1);
        }
    }
    close(to_pong);
    if wait(pong as u64) == 0 {
        write(CONSOLE, b"ping: done\n");
    }
    exit(0)
}
