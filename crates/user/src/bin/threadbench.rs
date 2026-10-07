//! `test=bench-threads`' init: times `thread` + `wait` + `close` round trips, then sends a thread of this process one
//! byte and reads it back over two pipes (`ping` and `pong` within one address space); prints each in ns.
#![no_std]
#![no_main]

use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::Relaxed;

use user::*;

const THREADS: u64 = 1000;
const ROUND_TRIPS: u64 = 100_000;

/// `pong`'s pipe ends: the read end of one, the write end of the other.
static PONG: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

extern "C" fn quit(_: u64) -> ! {
    thread_exit(0)
}

/// Echoes each byte until end of file, then exits with 0 (or the failed read's code).
extern "C" fn pong(_: u64) -> ! {
    let mut byte = [0];
    loop {
        let n = read(PONG[0].load(Relaxed), &mut byte);
        if n <= 0 {
            thread_exit(n as u64);
        }
        write(PONG[1].load(Relaxed), &byte);
    }
}

fn report(name: &[u8], start: u64, n: u64) {
    write(CONSOLE, name);
    write_u64(CONSOLE, (now_ns() - start) / n);
    write(CONSOLE, b" ns/round-trip\n");
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let Some(stack) = map(4096) else { exit(1) };
    let top = stack.as_mut_ptr() as u64 + 4096;
    let start = now_ns();
    for _ in 0..THREADS {
        let t = thread(quit, top, 0, 0);
        if t < 0 || wait(t as u64) != 0 {
            exit(1);
        }
        close(t as u64);
    }
    report(b"thread: ", start, THREADS);

    let (to_pong_read, to_pong) = pipe();
    let (from_pong, from_pong_write) = pipe();
    PONG[0].store(to_pong_read as u64, Relaxed);
    PONG[1].store(from_pong_write, Relaxed);
    let t = thread(pong, top, 0, 0);
    if to_pong_read < 0 || from_pong < 0 || t < 0 {
        exit(1);
    }
    let mut byte = [0];
    let start = now_ns();
    for _ in 0..ROUND_TRIPS {
        write(to_pong, &byte);
        if read(from_pong as u64, &mut byte) != 1 {
            exit(1);
        }
    }
    report(b"thread pipe: ", start, ROUND_TRIPS);
    close(to_pong);
    if wait(t as u64) != 0 {
        exit(1);
    }
    exit(0)
}
