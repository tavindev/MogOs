//! Spawned by `reaprace` with a pipe's write end (handle 0): starts a thread that spins, writes a byte to handle 0 once
//! it has, then spins too until it is killed, so both threads run when the kill lands.
#![no_std]
#![no_main]

use user::*;

extern "C" fn spin(_: u64) -> ! {
    loop {
        core::hint::spin_loop()
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let Some(stack) = map(4096) else { exit(1) };
    let top = stack.as_mut_ptr() as u64 + 4096;
    if thread(spin, top, 0, 0) < 0 {
        exit(1);
    }
    write(0, &[1]);
    spin(0)
}
