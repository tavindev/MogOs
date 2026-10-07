//! `test=bench-spawn`'s init: times `spawn` of `nop` + `wait` + `close` round trips, without and then with two
//! arguments; prints each in ns.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `nop`'s 9 frames (3 tables, text, stack, 4 kernel stack) and its argument page: exact with arguments.
const NOP_BUDGET: usize = 10;
const SPAWNS: u64 = 1000;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let nop = open(DIR, b"nop", 0) as u64;
    for (name, args) in [(&b"spawn: "[..], &b""[..]), (b"spawn+args: ", b"nop\0a\0")] {
        let start = now_ns();
        for _ in 0..SPAWNS {
            let process = spawn_at(nop, &[], NOP_BUDGET, u64::MAX, args);
            if process < 0 || wait(process as u64) != 0 {
                exit(1);
            }
            close(process as u64);
        }
        write(CONSOLE, name);
        write_u64(CONSOLE, (now_ns() - start) / SPAWNS);
        write(CONSOLE, b" ns/round-trip\n");
    }
    exit(0)
}
