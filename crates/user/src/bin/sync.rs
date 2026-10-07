//! msh's `sync`: commits the file system of the directory handle 1 (write right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |_| status(sync(1)))
}
