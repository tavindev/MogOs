//! msh's `mkdir <name>`: makes the directory `name` in the directory handle 1 (write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |args| status(mkdir(1, arg(args, 1))))
}
