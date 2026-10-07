//! `test=bench-syscalls`' init: times each syscall's fast path, one table entry per call. A batch makes the call many
//! times, in groups of up to `GROUP` between two counter reads, with any setup and undo outside the timed part; each
//! entry prints `bench <name>: <ns> ns`, the median of `BATCHES` batches' mean. Needs the MogFS root (handle 3).
#![no_std]
#![no_main]

use user::*;

/// init's handles: the boot archive, the MogFS root.
const ARCHIVE: u64 = 2;
const ROOT: u64 = 3;
const BATCHES: usize = 11;
/// Calls per counter read: amortizes the read, and fits the 16-entry handle table with init's 4.
const GROUP: usize = 8;
/// `nop`'s 9 frames and its argument page.
const NOP_BUDGET: usize = 10;
/// Each call: its name, calls per batch, and what each step of a group does.
const BENCHES: [(&str, u64, Bench); 27] = [
    ("console-write", 4096, |c, step| {
        if let Step::Timed(_) = step {
            ok(write(CONSOLE, &c.buf[..0]));
        }
    }),
    ("console-read", 4096, |c, step| {
        if let Step::Timed(_) = step {
            ok(read(CONSOLE, &mut c.buf[..0]));
        }
    }),
    ("pipe-write", 4096, |c, step| match step {
        Step::Timed(_) => _ = ok(write(c.write, &c.buf)),
        Step::After(_) => _ = ok(read(c.read, &mut c.buf)),
        _ => {}
    }),
    ("pipe-read", 4096, |c, step| match step {
        Step::Before(_) => _ = ok(write(c.write, &c.buf)),
        Step::Timed(_) => _ = ok(read(c.read, &mut c.buf)),
        _ => {}
    }),
    ("file-write", 256, |c, step| {
        if let Step::Timed(_) = step {
            ok(write(c.file, &c.buf));
        }
    }),
    ("file-read", 4096, |c, step| {
        if let Step::Timed(_) = step {
            ok(read(c.file, &mut c.buf));
        }
    }),
    ("dup", 4096, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(dup(CONSOLE, WRITE)),
        Step::After(i) => _ = ok(close(c.handles[i])),
        _ => {}
    }),
    ("close", 4096, |c, step| match step {
        Step::Before(i) => c.handles[i] = ok(dup(CONSOLE, WRITE)),
        Step::Timed(i) => _ = ok(close(c.handles[i])),
        _ => {}
    }),
    ("open", 4096, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(open(ROOT, b"f", 0)),
        Step::After(i) => _ = ok(close(c.handles[i])),
        _ => {}
    }),
    ("open-create", 256, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(open(ROOT, NAMES[i], CREATE)),
        Step::After(i) => {
            ok(close(c.handles[i]));
            ok(unlink(ROOT, NAMES[i]));
        }
        _ => {}
    }),
    ("open-trunc", 4096, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(open(ROOT, b"t", TRUNC)),
        Step::After(i) => _ = ok(close(c.handles[i])),
        _ => {}
    }),
    ("mkdir", 256, |_, step| match step {
        Step::Timed(i) => _ = ok(mkdir(ROOT, NAMES[i])),
        Step::After(i) => _ = ok(unlink(ROOT, NAMES[i])),
        _ => {}
    }),
    ("readdir", 4096, |c, step| {
        if let Step::Timed(_) = step {
            ok(readdir(ROOT, &mut c.list, 0));
        }
    }),
    ("unlink", 256, |_, step| match step {
        Step::Before(i) => _ = ok(close(ok(open(ROOT, NAMES[i], CREATE)))),
        Step::Timed(i) => _ = ok(unlink(ROOT, NAMES[i])),
        _ => {}
    }),
    ("rename", 256, |_, step| match step {
        Step::Timed(i) if i % 2 == 0 => _ = ok(rename(ROOT, b"r0", ROOT, b"r1")),
        Step::Timed(_) => _ = ok(rename(ROOT, b"r1", ROOT, b"r0")),
        _ => {}
    }),
    ("sync", 4096, |_, step| {
        if let Step::Timed(_) = step {
            ok(sync(ROOT));
        }
    }),
    ("sync-change", 32, |c, step| match step {
        Step::Before(_) => _ = ok(write(c.file, &c.buf)),
        Step::Timed(_) => _ = ok(sync(ROOT)),
        _ => {}
    }),
    ("map", 64, |_, step| {
        if let Step::Timed(_) = step {
            map(4096).unwrap_or_else(|| exit(1));
        }
    }),
    ("pipe", 512, |c, step| match step {
        Step::Timed(i) => {
            let (read, write) = pipe();
            (c.handles[2 * i], c.handles[2 * i + 1]) = (ok(read), write);
        }
        Step::After(i) => {
            ok(close(c.handles[2 * i]));
            ok(close(c.handles[2 * i + 1]));
        }
        _ => {}
    }),
    ("spawn", 64, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(spawn_at(c.nop, &[], NOP_BUDGET, u64::MAX, &[])),
        Step::After(i) => end(c.handles[i]),
        _ => {}
    }),
    ("spawn-args", 64, |c, step| match step {
        Step::Timed(i) => {
            c.handles[i] = ok(spawn_at(c.nop, &[], NOP_BUDGET, u64::MAX, b"nop\0a\0"))
        }
        Step::After(i) => end(c.handles[i]),
        _ => {}
    }),
    ("wait", 64, |c, step| match step {
        Step::Before(i) => {
            c.handles[i] = ok(spawn_at(c.nop, &[], NOP_BUDGET, u64::MAX, &[]));
            ok(kill(c.handles[i]));
        }
        Step::Timed(i) => _ = ok(wait(c.handles[i])),
        Step::After(i) => _ = ok(close(c.handles[i])),
    }),
    ("kill", 64, |c, step| match step {
        Step::Before(i) => c.handles[i] = ok(spawn_at(c.nop, &[], NOP_BUDGET, u64::MAX, &[])),
        Step::Timed(i) => _ = ok(kill(c.handles[i])),
        Step::After(i) => {
            ok(wait(c.handles[i]));
            ok(close(c.handles[i]));
        }
    }),
    ("mutex", 4096, |c, step| match step {
        Step::Timed(i) => c.handles[i] = ok(mutex()),
        Step::After(i) => _ = ok(close(c.handles[i])),
        _ => {}
    }),
    ("lock", 4096, |c, step| match step {
        Step::Before(i) => c.handles[i] = ok(mutex()),
        Step::Timed(i) => _ = ok(lock(c.handles[i])),
        Step::After(i) => {
            ok(unlock(c.handles[i]));
            ok(close(c.handles[i]));
        }
    }),
    ("unlock", 4096, |c, step| match step {
        Step::Before(i) => {
            c.handles[i] = ok(mutex());
            ok(lock(c.handles[i]));
        }
        Step::Timed(i) => _ = ok(unlock(c.handles[i])),
        Step::After(i) => _ = ok(close(c.handles[i])),
    }),
    ("enosys", 4096, |_, step| {
        if let Step::Timed(_) = step {
            // SAFETY: there is no syscall 18, so the kernel touches no memory.
            _ = unsafe { raw(18, [0; 7]) };
        }
    }),
];

/// Distinct names for the files and directories a group makes.
const NAMES: [&[u8]; GROUP] = [b"n0", b"n1", b"n2", b"n3", b"n4", b"n5", b"n6", b"n7"];

/// One call of a group: `Before` and `After` (setup and undo) run untimed around the `Timed` calls.
#[derive(Clone, Copy)]
enum Step {
    Before(usize),
    Timed(usize),
    After(usize),
}

type Bench = fn(&mut Ctx, Step);

struct Ctx {
    nop: u64,
    file: u64,
    read: u64,
    write: u64,
    handles: [u64; 2 * GROUP],
    buf: [u8; 64],
    list: [u8; 512],
}

/// `result` as a handle or count; a failure exits, so the bench's line is missing.
fn ok(result: i64) -> u64 {
    if result < 0 {
        exit(1);
    }
    result as u64
}

/// Kills, reaps and closes a spawned `nop`.
fn end(process: u64) {
    ok(kill(process));
    ok(wait(process));
    ok(close(process));
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let (pipe_read, pipe_write) = pipe();
    let mut c = Ctx {
        nop: ok(open(ARCHIVE, b"nop", 0)),
        file: ok(open(ROOT, b"f", CREATE)),
        read: ok(pipe_read),
        write: pipe_write,
        handles: [0; 2 * GROUP],
        buf: [0x5a; 64],
        list: [0; 512],
    };
    ok(write_at(c.file, &c.buf, 0));
    ok(close(ok(open(ROOT, b"t", CREATE))));
    ok(close(ok(open(ROOT, b"r0", CREATE))));
    ok(sync(ROOT));
    let freq = ticks_per_s() as u128;
    for (name, calls, bench) in BENCHES {
        // Pipes, spawns and sync-change make fewer calls per group: two handles each, 4 children, one change each.
        let group = match name {
            "pipe" | "spawn" | "spawn-args" | "wait" | "kill" => 4,
            "sync-change" => 1,
            _ => GROUP,
        };
        let mut batches = [0; BATCHES];
        for batch in &mut batches {
            let mut spent = 0;
            for _ in 0..calls / group as u64 {
                (0..group).for_each(|i| bench(&mut c, Step::Before(i)));
                let start = ticks();
                (0..group).for_each(|i| bench(&mut c, Step::Timed(i)));
                spent += ticks() - start;
                (0..group).for_each(|i| bench(&mut c, Step::After(i)));
            }
            // Tenths of a nanosecond per call.
            *batch = (spent as u128 * 10_000_000_000 / freq / calls as u128) as u64;
        }
        batches.sort_unstable();
        let tenths = batches[BATCHES / 2];
        write(CONSOLE, b"bench ");
        write(CONSOLE, name.as_bytes());
        write(CONSOLE, b": ");
        write_u64(CONSOLE, tenths / 10);
        write(CONSOLE, b".");
        write_u64(CONSOLE, tenths % 10);
        write(CONSOLE, b" ns\n");
    }
    exit(0)
}
