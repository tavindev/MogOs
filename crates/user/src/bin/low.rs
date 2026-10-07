//! `test=pi`'s L: spawned with the console, a mutex, a pipe write end and a pipe read end. Locks the mutex, reports it,
//! holds it until the read end has data, then unlocks and relocks it.
#![no_std]
#![no_main]

use user::*;

const MUTEX: u64 = 1;
const LOCKED: u64 = 2;
const GO: u64 = 3;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    if lock(MUTEX) == 0 {
        write(CONSOLE, b"L: locked\n");
    }
    if lock(MUTEX) == EDEADLK {
        write(CONSOLE, b"L: relock: EDEADLK\n");
    }
    write(LOCKED, b"x");
    read(GO, &mut [0; 2]);
    write(CONSOLE, b"L: unlocking\n");
    unlock(MUTEX);
    if lock(MUTEX) == 0 {
        write(CONSOLE, b"L: relocked\n");
    }
    unlock(MUTEX);
    exit(0)
}
