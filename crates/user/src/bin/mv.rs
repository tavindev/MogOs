//! msh's `mv <from> <to>`: moves the entry `from` in the directory handle 1 to `to` in the directory handle 2 (both
//! write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |args| {
        status(rename(1, arg(args, 1), 2, arg(args, 2)))
    })
}
