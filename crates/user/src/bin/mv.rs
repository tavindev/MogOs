//! msh's `mv <from> <to>`: moves the entry `from` in the directory handle 1 to `to` in the directory handle 2 (both
//! write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    status(rename(1, arg(args, 1), 2, arg(args, 2)))
}
