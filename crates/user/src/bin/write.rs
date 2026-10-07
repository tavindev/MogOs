//! msh's `write <name> <text>`: replaces the file `name` in the directory handle 1 (read and write rights) with the
//! words of `text`, separated by spaces, and a newline.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let file = open(1, arg(args, 1), CREATE | TRUNC);
    if file < 0 {
        return status(file);
    }
    let written = write_words(file as u64, &args[2.min(args.len())..], 0);
    close(file as u64);
    status(written)
}
