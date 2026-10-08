//! `test=reap-race`'s init: spawns `nop` again and again, killing every other one while it may be ending on another
//! core, and waits for each. A wait returns only once the child is released, so each spawn gets the whole budget back
//! and may reuse the index (and ASID) the last one freed.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `nop`'s 9 frames (3 tables, text, stack, 4 kernel stack).
const NOP_BUDGET: usize = 9;
const ROUNDS: usize = 500;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let exe = open(DIR, b"nop", 0) as u64;
    for i in 0..ROUNDS {
        let child = spawn(exe, &[], NOP_BUDGET);
        if child < 0 {
            exit(1);
        }
        if i % 2 == 1 {
            kill(child as u64);
        }
        let code = wait(child as u64);
        if (code != 0 && code != KILLED) || close(child as u64) != 0 {
            exit(2);
        }
    }
    write(CONSOLE, b"reap-race: done\n");
    exit(0)
}
