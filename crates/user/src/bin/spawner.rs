//! `test=spawn`'s init: spawns `child` with only the console, after failing spawns that must move nothing.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
const CHILD_BUDGET: usize = 12;
/// One short of `child`'s 10 frames (3 tables, text, data, stack, 4 kernel stack): fails at the last, the kernel stack.
const ONE_FRAME_SHORT: usize = 9;

/// Writes `line` through `console` if `ok`; otherwise exits, so a wrong result shows as missing lines.
fn check(console: u64, ok: bool, line: &[u8]) {
    if !ok {
        exit(1);
    }
    write(console, line);
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let console = dup(CONSOLE, WRITE) as u64;
    let missing = open(DIR, b"missing", EXEC);
    check(console, missing == ENOENT, b"S: open missing: ENOENT\n");
    let empty = write(console, &[]);
    check(console, empty == 0, b"S: empty write: 0\n");
    let bad = open(DIR, b"bad", EXEC) as u64;
    let spawned = spawn(bad, &[CONSOLE], CHILD_BUDGET);
    check(console, spawned == ENOEXEC, b"S: spawn non-ELF: ENOEXEC\n");
    let child = open(DIR, b"child", EXEC) as u64;
    let spawned = spawn(child, &[CONSOLE], 1 << 20);
    check(
        console,
        spawned == ENOMEM,
        b"S: spawn over budget: ENOMEM\n",
    );
    let spawned = spawn(child, &[], 1 << 20);
    check(
        console,
        spawned == ENOMEM,
        b"S: spawn without handles over budget: ENOMEM\n",
    );
    let spawned = spawn(child, &[CONSOLE], ONE_FRAME_SHORT);
    check(
        console,
        spawned == ENOMEM,
        b"S: spawn one frame short: ENOMEM\n",
    );
    write(CONSOLE, b"S: console not moved\n");
    let spawned = spawn(child, &[CONSOLE], CHILD_BUDGET);
    check(
        console,
        spawned >= 0,
        b"S: spawned child with the console\n",
    );
    let moved = write(CONSOLE, b"S: still mine\n");
    check(console, moved == EBADF, b"S: moved console: EBADF\n");
    exit(0)
}
