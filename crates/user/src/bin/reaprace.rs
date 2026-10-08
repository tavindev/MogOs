//! `test=reap-race`'s init: spawns `nop` again and again, killing every other one while it may be ending on another
//! core, and every third round `spinpair`, killed once both its threads spin, so they end on two cores, the first
//! parking its own stack while the other releases the process; it waits for each. A wait returns only once the child
//! is released, so each spawn gets the whole budget back and may reuse the index (and ASID) the last one freed.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `nop`'s 9 frames (3 tables, text, stack, 4 kernel stack).
const NOP_BUDGET: usize = 9;
/// `spinpair`'s 9, its thread's stack page and map table, and its 4 kernel stack frames, with 3 to spare.
const PAIR_BUDGET: usize = 18;
const ROUNDS: usize = 500;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let nop = open(DIR, b"nop", 0) as u64;
    let pair = open(DIR, b"spinpair", 0) as u64;
    let (read_end, write_end) = pipe();
    if read_end < 0 {
        exit(1);
    }
    for i in 0..ROUNDS {
        let child = match i % 3 {
            2 => {
                let end = dup(write_end, WRITE | TRANSFER);
                let child = spawn(pair, &[end as u64], PAIR_BUDGET);
                // Both its threads spin once the byte is here.
                if child >= 0 && read(read_end as u64, &mut [0]) != 1 {
                    exit(1);
                }
                child
            }
            _ => spawn(nop, &[], NOP_BUDGET),
        };
        if child < 0 {
            exit(1);
        }
        if i % 2 == 1 || i % 3 == 2 {
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
