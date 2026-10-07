//! `test=bench-fs`'s init: checks that `unlink` refuses a file a handle reaches (`EBUSY`) and that a read at or past
//! the file's end and a `readdir` from past the last entry return 0, then times `open(CREATE | TRUNC)` + a 100-byte write + `sync` round trips, then `open` +
//! `close` round trips, on a file in the root directory; prints each in ns.
#![no_std]
#![no_main]

use user::*;

/// init's root directory handle, present when a disk is mounted.
const ROOT: u64 = 3;
const SYNCS: u64 = 1000;
const OPENS: u64 = 100_000;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let data = [0x5a; 100];
    let held = open(ROOT, b"held", CREATE);
    let mut buf = [0; 4];
    let mut list = [0; 64];
    if held < 0
        || write(held as u64, b"kept") != 4
        || unlink(ROOT, b"held") != EBUSY
        || read(held as u64, &mut buf) != 4
        || buf != *b"kept"
        || read_at(held as u64, &mut buf, 4) != 0
        || read_at(held as u64, &mut buf, 1 << 40) != 0
        || readdir(ROOT, &mut list, 1) != 0
        || readdir(ROOT, &mut list, 1 << 40) != 0
    {
        exit(1);
    }
    close(held as u64);
    if unlink(ROOT, b"held") != 0 {
        exit(1);
    }
    let start = now_ns();
    for _ in 0..SYNCS {
        let file = open(ROOT, b"f", CREATE | TRUNC);
        if file < 0 || write(file as u64, &data) != 100 || sync(ROOT) != 0 {
            exit(1);
        }
        close(file as u64);
    }
    report(b"open+write+sync: ", (now_ns() - start) / SYNCS);
    let start = now_ns();
    for _ in 0..OPENS {
        let file = open(ROOT, b"f", 0);
        if file < 0 {
            exit(1);
        }
        close(file as u64);
    }
    report(b"open+close: ", (now_ns() - start) / OPENS);
    exit(0)
}

fn report(name: &[u8], ns: u64) {
    write(CONSOLE, name);
    write_u64(CONSOLE, ns);
    write(CONSOLE, b" ns/round-trip\n");
}
