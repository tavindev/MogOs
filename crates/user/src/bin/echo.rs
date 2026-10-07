//! msh's `echo`: prints its arguments, separated by spaces.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    write_words(CONSOLE, &args[1.min(args.len())..], 0);
    0
}
