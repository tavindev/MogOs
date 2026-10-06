//! `test=pipe`'s init: reads a pipe whose only writer left is `writer`, waits for it, then spawns it again into
//! its old slot.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// Over half of the 15 frames left of 25 after the reader's 9 (3 tables, text, stack, 4 kernel stack) and the pipe's
/// page, so a second spawn fits only once `wait` gave the first writer's budget back; `writer` needs 9.
const CHILD_BUDGET: usize = 12;

/// Writes `line` to the console if `ok`; otherwise exits, so a wrong result shows as missing lines.
fn check(ok: bool, line: &[u8]) {
    if !ok {
        exit(1);
    }
    write(CONSOLE, line);
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let (read_end, write_end) = pipe();
    let exe = open(DIR, b"writer", EXEC) as u64;
    let console = dup(CONSOLE, WRITE | TRANSFER) as u64;
    let writer_end = dup(write_end, WRITE | TRANSFER) as u64;
    let child = spawn(exe, &[console, writer_end], CHILD_BUDGET);
    check(read_end >= 0 && child >= 0, b"R: spawned writer\n");
    let (read_end, child) = (read_end as u64, child as u64);
    check(close(write_end) == 0, b"R: reading the empty pipe\n");
    let mut buf = [0; 16];
    let n = read(read_end, &mut buf);
    check(n > 0, b"R: read: ");
    write(CONSOLE, &buf[..n as usize]);
    check(read(read_end, &mut buf) == 0, b"R: EOF\n");
    check(wait(child) == 7, b"R: writer exited with 7\n");
    let again = spawn(exe, &[], CHILD_BUDGET);
    check(again >= 0, b"R: budget returned: spawned writer again\n");
    check(wait(child) == EBADF, b"R: stale process handle: EBADF\n");
    check(wait(again as u64) == 7, b"R: second writer exited with 7\n");
    exit(0)
}
