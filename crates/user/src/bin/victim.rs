//! Spawned by `threads` with a pipe's write end (handle 0): starts a thread that spins forever, writes a byte to
//! handle 0, then blocks reading an empty pipe of its own until it is killed.
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
    let (read_end, _write_end) = pipe();
    if read_end < 0 || thread(spin, top, 0, 0) < 0 {
        exit(1);
    }
    write(0, &[1]);
    read(read_end as u64, &mut [0]);
    exit(1)
}
