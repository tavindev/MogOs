//! msh's `sync`: commits the file system of the directory handle 1 (write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(_: &[&[u8]]) -> u64 {
    status(sync(1))
}
