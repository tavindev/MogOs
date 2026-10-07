//! msh's `echo`: prints its arguments, separated by spaces.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |args| {
        write_words(CONSOLE, &args[1.min(args.len())..], 0);
        0
    })
}
