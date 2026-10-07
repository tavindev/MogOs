//! `test=wait`'s init: child A exits before child B is spawned, and `wait` still reports both exit codes and both
//! budgets, so A's slot stayed reserved; closing a third, exited child's handle also returns its budget.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// `writer`'s 9 frames (3 tables, text, stack, 4 kernel stack).
const A_BUDGET: usize = 9;
/// `child`'s 10 frames (`writer`'s and a data page).
const B_BUDGET: usize = 10;

/// Writes `line` to the console if `ok`; otherwise exits, so a wrong result shows as missing lines.
fn check(ok: bool, line: &[u8]) {
    if !ok {
        exit(1);
    }
    write(CONSOLE, line);
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let console = dup(CONSOLE, WRITE | TRANSFER) as u64;
    let (read_end, write_end) = pipe();
    let silent = dup(write_end, TRANSFER) as u64;
    close(write_end);
    let writer = open(DIR, b"writer", EXEC) as u64;
    let a = spawn(writer, &[silent], A_BUDGET);
    check(read_end >= 0 && a >= 0, b"P: spawned A\n");
    // A holds the only write end, without write: this read blocks until A's exit closes it.
    let mut buf = [0];
    check(
        read(read_end as u64, &mut buf) == 0,
        b"P: EOF once A exits\n",
    );
    close(read_end as u64);
    let child = open(DIR, b"child", EXEC) as u64;
    let b = spawn(child, &[console], B_BUDGET);
    check(b >= 0, b"P: spawned B\n");
    check(wait(a as u64) == 7, b"P: A exited with 7\n");
    check(wait(b as u64) == 0, b"P: B exited with 0\n");
    let both = spawn(child, &[], A_BUDGET + B_BUDGET);
    check(
        both >= 0 && wait(both as u64) == 0,
        b"P: both budgets returned\n",
    );
    // A third child exits unwaited for: closing its handle returns its budget too.
    let (read_end, write_end) = pipe();
    let silent = dup(write_end, TRANSFER) as u64;
    close(write_end);
    let third = spawn(writer, &[silent], A_BUDGET);
    check(
        third >= 0 && read(read_end as u64, &mut buf) == 0,
        b"P: third child exited\n",
    );
    close(read_end as u64);
    close(third as u64);
    let both = spawn(child, &[], A_BUDGET + B_BUDGET);
    check(
        both >= 0 && wait(both as u64) == 0,
        b"P: close returned its budget\n",
    );
    exit(0)
}
