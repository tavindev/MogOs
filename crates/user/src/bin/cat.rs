//! msh's `cat`: prints the file handle 1 (read right).
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    start(argc, len, |_| {
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
    })
}
