//! `test=refund`'s init: kills its child `nop`, which stays a zombie its handle holds, starts a thread blocked on an
//! empty pipe, hands that thread's handle to `refundc`, and ends its main thread. `refundc` kills the thread, the
//! process's last, and checks that the zombie's frames were not credited to it.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `nop`'s 9 frames (3 tables, text, stack, 4 kernel stack).
const NOP_BUDGET: usize = 9;
/// `refundc`'s 9 frames and 5 to map.
const KILLER_BUDGET: usize = 14;

extern "C" fn block(read_end: u64) -> ! {
    read(read_end, &mut [0]);
    exit(1)
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let nop = spawn(open(DIR, b"nop", 0) as u64, &[], NOP_BUDGET);
    let Some(stack) = map(4096) else { exit(1) };
    let (read_end, _write_end) = pipe();
    if nop < 0 || kill(nop as u64) != 0 || read_end < 0 {
        exit(1);
    }
    let top = stack.as_mut_ptr() as u64 + 4096;
    let t = thread(block, top, 0, read_end as u64);
    let console = dup(CONSOLE, WRITE | TRANSFER);
    let killer = open(DIR, b"refundc", 0) as u64;
    if t < 0 || console < 0 || spawn(killer, &[console as u64, t as u64], KILLER_BUDGET) < 0 {
        exit(1);
    }
    thread_exit(0)
}
