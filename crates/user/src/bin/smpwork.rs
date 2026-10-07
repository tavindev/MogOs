//! A `test=bench-smp` worker, one process of k: with the argument `syscall` it makes `SYSCALLS` 0-byte console writes,
//! with `pipe` `ROUND_TRIPS` one-byte round trips with its own `pong`, with `spawn` `SPAWNS` spawns and waits of
//! `nop`; the kernel times the k workers from their spawn until all exited. Exits 1 on any failure.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// Must equal the kernel's `SMP_SYSCALLS`, `SMP_ROUND_TRIPS` and `SMP_SPAWNS`, which it divides the time by.
const SYSCALLS: u64 = 20_000;
const ROUND_TRIPS: u64 = 2_000;
const SPAWNS: u64 = 200;
/// `pong`'s 9 frames; `nop`'s 9.
const PONG_BUDGET: usize = 9;
const NOP_BUDGET: usize = 9;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let ok = match arg(args, 1) {
        b"syscall" => (0..SYSCALLS).all(|_| write(CONSOLE, &[]) == 0),
        b"pipe" => pipe_round_trips(),
        b"spawn" => {
            let nop = open(DIR, b"nop", 0) as u64;
            (0..SPAWNS).all(|_| {
                let process = spawn(nop, &[], NOP_BUDGET);
                process >= 0 && wait(process as u64) == 0 && close(process as u64) == 0
            })
        }
        _ => false,
    };
    (!ok) as u64
}

/// One byte to `pong` and back over two pipes, `ROUND_TRIPS` times.
fn pipe_round_trips() -> bool {
    let (to_pong_read, to_pong) = pipe();
    let (from_pong, from_pong_write) = pipe();
    let exe = open(DIR, b"pong", 0) as u64;
    let pong = spawn(exe, &[to_pong_read as u64, from_pong_write], PONG_BUDGET);
    if to_pong_read < 0 || from_pong < 0 || pong < 0 {
        return false;
    }
    let mut byte = [0];
    let echoed = (0..ROUND_TRIPS)
        .all(|_| write(to_pong, &byte) == 1 && read(from_pong as u64, &mut byte) == 1);
    close(to_pong);
    echoed && wait(pong as u64) == 0
}
