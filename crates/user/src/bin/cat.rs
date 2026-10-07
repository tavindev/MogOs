//! msh's `cat`: prints the file handle 1 (read right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(_: &[&[u8]]) -> u64 {
    let mut buf = [0; 512];
    let mut offset = 0;
    loop {
        let n = read_at(1, &mut buf, offset);
        if n <= 0 {
            return status(n);
        }
        write(CONSOLE, &buf[..n as usize]);
        offset += n as u64;
    }
}
