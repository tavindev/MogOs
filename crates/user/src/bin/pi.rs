//! `test=pi`'s init (top priority): `low` (1) holds a mutex that `high` (3) blocks on while `mid` (2) is ready to spin
//! forever; `high` acquires it only if `low` inherits its priority. Then kills `mid`.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// Each child's 9 frames (3 tables, text, stack, 4 kernel stack).
const CHILD_BUDGET: usize = 9;

/// Writes `line` to the console if `ok`; otherwise exits, so a wrong result shows as missing lines.
fn check(ok: bool, line: &[u8]) {
    if !ok {
        exit(1);
    }
    write(CONSOLE, line);
}

fn spawn_child(name: &[u8], handles: &[u64], priority: u64) -> u64 {
    spawn_at(
        open(DIR, name, EXEC) as u64,
        handles,
        CHILD_BUDGET,
        priority,
    ) as u64
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let mutex = mutex() as u64;
    let (locked, locked_write) = pipe();
    let (go_read, go) = pipe();
    let console = dup(CONSOLE, WRITE | TRANSFER) as u64;
    let shared = dup(mutex, TRANSFER) as u64;
    let low = spawn_child(b"low", &[console, shared, locked_write, go_read as u64], 1);
    // Blocks until `low` owns the mutex.
    read(locked as u64, &mut [0]);
    let console = dup(CONSOLE, WRITE | TRANSFER) as u64;
    let mid = spawn_child(b"mid", &[console], 2);
    let console = dup(CONSOLE, WRITE | TRANSFER) as u64;
    let high = spawn_child(b"high", &[console, mutex], 3);
    // `low` may unlock now, but runs only while it outranks `mid`.
    write(go, b"go");
    check(wait(high) == 0, b"P: high exited\n");
    check(kill(mid) == 0 && wait(mid) == KILLED, b"P: mid killed\n");
    check(wait(low) == 0, b"P: low exited\n");
    exit(0)
}
