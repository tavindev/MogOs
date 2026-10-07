//! msh's `rm <name>`: removes the file or empty directory `name` from the directory handle 1 (write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |args| status(unlink(1, arg(args, 1))))
}
