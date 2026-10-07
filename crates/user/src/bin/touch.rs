//! msh's `touch <name>`: makes the file `name` in the directory handle 1 (read and write rights) if it is missing.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let file = open(1, arg(args, 1), CREATE);
    if file >= 0 {
        close(file as u64);
    }
    status(file)
}
