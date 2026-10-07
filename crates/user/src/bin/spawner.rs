//! `test=spawn`'s init: spawns `child` with only the console and the most arguments `spawn` takes, after failing
//! spawns that must move nothing.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
const CHILD_BUDGET: usize = 12;
/// `child`'s 10 frames, one short once its arguments need their own stack page.
const WITHOUT_ARGS_PAGE: usize = 10;
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
    let missing = open(DIR, b"missing", 0);
    check(console, missing == ENOENT, b"S: open missing: ENOENT\n");
    let empty = write(console, &[]);
    check(console, empty == 0, b"S: empty write: 0\n");
    let bad = open(DIR, b"bad", 0) as u64;
    let spawned = spawn(bad, &[CONSOLE], CHILD_BUDGET);
    check(console, spawned == ENOEXEC, b"S: spawn non-ELF: ENOEXEC\n");
    let child = open(DIR, b"child", 0) as u64;
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
    // The most `spawn` takes: 32 arguments in 4096 bytes, the last one long.
    let args = map(2 * 4096).unwrap_or_else(|| exit(1));
    args[..10].copy_from_slice(b"child\0a b\0");
    args[10..68].chunks_mut(2).for_each(|arg| arg[0] = b'x');
    args[68..4095].fill(b'y');
    let spawned = spawn_at(child, &[CONSOLE], CHILD_BUDGET, u64::MAX, &args[..4097]);
    check(
        console,
        spawned == E2BIG,
        b"S: spawn 4097 bytes of args: E2BIG\n",
    );
    let mut many = [0; 66];
    many.chunks_mut(2).for_each(|arg| arg[0] = b'x');
    let spawned = spawn_at(child, &[CONSOLE], CHILD_BUDGET, u64::MAX, &many);
    check(console, spawned == E2BIG, b"S: spawn 33 args: E2BIG\n");
    let spawned = spawn_at(child, &[CONSOLE], CHILD_BUDGET, u64::MAX, b"child");
    check(
        console,
        spawned == EINVAL,
        b"S: spawn args without a NUL: EINVAL\n",
    );
    let spawned = spawn_at(child, &[CONSOLE], WITHOUT_ARGS_PAGE, u64::MAX, b"child\0");
    check(
        console,
        spawned == ENOMEM,
        b"S: spawn with args one frame short: ENOMEM\n",
    );
    write(CONSOLE, b"S: console not moved\n");
    let spawned = spawn_at(child, &[CONSOLE], CHILD_BUDGET, u64::MAX, &args[..4096]);
    check(
        console,
        spawned >= 0,
        b"S: spawned child with the console\n",
    );
    let moved = write(CONSOLE, b"S: still mine\n");
    check(console, moved == EBADF, b"S: moved console: EBADF\n");
    exit(0)
}
