//! Spawned by `refund` with the console (handle 0) and a thread handle (1) to the last thread of `refund`, which holds
//! a zombie child: maps pages until its budget runs out, kills that thread (and so `refund`), and prints how many
//! more pages it can map after: 0, as the zombie's frames are not its own.
#![no_std]
#![no_main]

use user::*;

/// Pages `map` takes until it fails.
fn pages() -> u64 {
    let mut n = 0;
    while map(4096).is_some() {
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    pages();
    if kill(1) != 0 || wait(1) != KILLED {
        exit(1);
    }
    write(0, b"R: the killer gained ");
    write_u64(0, pages());
    write(0, b" pages\n");
    exit(0)
}
