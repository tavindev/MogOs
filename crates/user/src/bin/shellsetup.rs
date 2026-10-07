//! `test=bench-shell`'s first init: makes the fixtures msh's timed commands use in the MogFS root (handle 3):
//! directories `d1`, `d100`, `d390` with that many empty files, `small` (4 KiB), `big` (the largest file MogFS v1
//! holds, 57232 bytes) and `a` (empty, for `mv`). With msh's `w` and `m`, that is 500 of MogFS's 504 inodes.
#![no_std]
#![no_main]

use user::*;

const ROOT: u64 = 3;
/// 14 blocks of 4088 bytes each.
const BIG: u64 = 14 * 4088;
/// One line of file content: no line starts with `bench `.
const LINE: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.\n";

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    for (dir, files) in [(&b"d1"[..], 1), (b"d100", 100), (b"d390", 390)] {
        check(mkdir(ROOT, dir));
        let dir = check(open(ROOT, dir, 0));
        for i in 0..files {
            let name = [
                b'f',
                b'0' + (i / 100) as u8,
                b'0' + (i / 10 % 10) as u8,
                b'0' + (i % 10) as u8,
            ];
            check(close(check(open(dir, &name, CREATE))));
        }
        check(close(dir));
    }
    // Mapped, not on the one-page stack.
    let chunk = map(4096).unwrap_or_else(|| exit(1));
    for (i, b) in chunk.iter_mut().enumerate() {
        *b = LINE[i % LINE.len()];
    }
    for (name, size) in [(&b"small"[..], 4096), (b"big", BIG), (b"a", 0)] {
        let file = check(open(ROOT, name, CREATE));
        for offset in (0..size).step_by(chunk.len()) {
            let len = (size - offset).min(chunk.len() as u64) as usize;
            check(write_at(file, &chunk[..len], offset));
        }
        // A newline last, so msh's `bench` line after a `cat` starts a line.
        if size > 0 {
            check(write_at(file, b"\n", size - 1));
        }
        check(close(file));
    }
    exit(0)
}

/// `result` as a handle or count; a failure exits, so msh's commands then fail visibly.
fn check(result: i64) -> u64 {
    if result < 0 {
        exit(1);
    }
    result as u64
}
