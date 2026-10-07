//! Reads two lines from the console, then prints them back.
#![no_std]
#![no_main]

use user::*;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    write(CONSOLE, b"E: ready\n");
    let mut lines = [[0; 64]; 2];
    let lens = lines
        .each_mut()
        .map(|line| read(CONSOLE, line).max(0) as usize);
    for (line, len) in lines.iter().zip(lens) {
        write(CONSOLE, b"got: ");
        write(CONSOLE, &line[..len]);
    }
    exit(0)
}
