//! `test=pipe`'s init: reads a pipe whose only writer left is `writer`, waits for it, then spawns it again into
//! its old slot with a write end it cannot write, waits for it while it has yet to run, and reads end of file; then
//! moves 8 KiB through a pipe.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// Over half of the 14 frames left of 25 after the reader's 10 (3 tables, 2 text, stack, 4 kernel stack) and the
/// pipe's page, so a second spawn (after a second pipe's page) fits only once `wait` gave the first writer's budget
/// back; `writer` needs 9.
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
    // The second writer gets the only write end of a new pipe, without the write right: it runs while this waits.
    let (read_end, write_end) = pipe();
    let silent = dup(write_end, TRANSFER) as u64;
    close(write_end);
    let again = spawn(exe, &[silent], CHILD_BUDGET);
    check(
        read_end >= 0 && again >= 0,
        b"R: budget returned: spawned writer again\n",
    );
    check(wait(child) == EBADF, b"R: stale process handle: EBADF\n");
    check(wait(again as u64) == 7, b"R: second writer exited with 7\n");
    let eof = read(read_end as u64, &mut buf) == 0;
    check(eof, b"R: EOF after the second writer exited\n");
    let Some(big) = map(8192) else { exit(1) };
    let (read_end, write_end) = pipe();
    check(write(write_end, big) == 4096, b"R: 8 KiB write: 4096\n");
    check(read(read_end as u64, big) == 4096, b"R: 8 KiB read: 4096\n");
    exit(0)
}
