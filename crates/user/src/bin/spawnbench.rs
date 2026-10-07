//! `test=bench-spawn`'s init: times `spawn` of `nop` with two arguments + `wait` + `close` round trips; prints ns.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `nop`'s 9 frames (3 tables, text, stack, 4 kernel stack) and its argument page: exact.
const NOP_BUDGET: usize = 10;
const SPAWNS: u64 = 1000;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let nop = open(DIR, b"nop", 0) as u64;
    let start = now_ns();
    for _ in 0..SPAWNS {
        let process = spawn_at(nop, &[], NOP_BUDGET, u64::MAX, b"nop\0a\0");
        if process < 0 || wait(process as u64) != 0 {
            exit(1);
        }
        close(process as u64);
    }
    write(CONSOLE, b"spawn: ");
    write_u64(CONSOLE, (now_ns() - start) / SPAWNS);
    write(CONSOLE, b" ns/round-trip\n");
    exit(0)
}
