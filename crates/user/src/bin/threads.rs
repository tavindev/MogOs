//! `test=threads`' init: four threads each add 1 to a shared counter `ADDS` times and exit with their TPIDR_EL0, which
//! the joins check; a child with one thread spinning and one blocked on a pipe is killed; a spinning thread is killed
//! through its handle; then the main thread exits first, and the last thread ends the process by killing itself.
#![no_std]
#![no_main]

use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::Relaxed;

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
const THREADS: usize = 4;
const ADDS: u64 = 100_000;
/// `victim`'s 9 frames (3 tables, text, stack, 4 kernel stack), its pipe page, and its spinner's stack page, map
/// table and 4 kernel stack frames, with 2 to spare.
const VICTIM_BUDGET: usize = 18;

static COUNT: AtomicU64 = AtomicU64::new(0);
/// The last thread's own handle, stored once the main thread has printed its last line.
static LAST: AtomicU64 = AtomicU64::new(0);

/// Writes `line` to the console if `ok`; otherwise exits, so a wrong result shows as a missing line.
fn check(ok: bool, line: &[u8]) {
    if !ok {
        exit(1);
    }
    write(CONSOLE, line);
}

/// `fetch_add` is an `ldxr`/`stxr` loop on this target.
extern "C" fn add(_: u64) -> ! {
    for _ in 0..ADDS {
        COUNT.fetch_add(1, Relaxed);
    }
    thread_exit(tls())
}

extern "C" fn spin(_: u64) -> ! {
    loop {
        core::hint::spin_loop()
    }
}

/// Writes a byte to the pipe end `write_end`, then exits with 7.
extern "C" fn signal(write_end: u64) -> ! {
    write(write_end, &[1]);
    thread_exit(7)
}

extern "C" fn last(_: u64) -> ! {
    let mut me = 0;
    while me == 0 {
        me = LAST.load(Relaxed);
    }
    write(CONSOLE, b"T: the last thread ends the process\n");
    kill(me);
    exit(1)
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let Some(stacks) = map(THREADS * 4096) else {
        exit(1)
    };
    let base = stacks.as_mut_ptr() as u64;
    let mut threads = [0; THREADS];
    for (i, t) in threads.iter_mut().enumerate() {
        let tls = i as u64 + 1;
        *t = thread(add, base + tls * 4096, tls, 0);
        if *t < 0 {
            exit(1);
        }
    }
    write(CONSOLE, b"T: joined");
    for t in threads {
        write(CONSOLE, b" ");
        write_u64(CONSOLE, wait(t as u64) as u64);
        close(t as u64);
    }
    write(CONSOLE, b"\nT: count ");
    write_u64(CONSOLE, COUNT.load(Relaxed));
    write(CONSOLE, b"\n");

    let (ready, ready_write) = pipe();
    let victim = spawn(
        open(DIR, b"victim", 0) as u64,
        &[ready_write],
        VICTIM_BUDGET,
    );
    let mut byte = [0];
    // The victim writes once its spinner is started, then blocks on its own empty pipe (unless a tick lands between).
    check(
        ready >= 0
            && victim >= 0
            && read(ready as u64, &mut byte) == 1
            && kill(victim as u64) == 0
            && wait(victim as u64) == KILLED,
        b"T: killed a process with a spinning and a blocked thread\n",
    );

    let t = thread(spin, base + 4096, 0, 0);
    check(
        t >= 0 && kill(t as u64) == 0 && wait(t as u64) == KILLED && close(t as u64) == 0,
        b"T: a killed thread joins with KILLED\n",
    );
    // More rounds than free slots, so a slot kept after its last handle closes would fail a later `thread`; the first
    // is joined through its second handle, after its first closed. Two stacks, so no ending thread shares one.
    let (signals, signal_end) = pipe();
    for i in 0..9 {
        let t = thread(signal, base + (3 + i % 2) * 4096, 0, signal_end);
        let d = dup(t as u64, WAIT);
        let ended = signals >= 0 && t >= 0 && d >= 0 && read(signals as u64, &mut byte) == 1;
        let closed = close(t as u64) == 0 && (i > 0 || wait(d as u64) == 7) && close(d as u64) == 0;
        if !(ended && closed) {
            exit(1);
        }
    }
    write(
        CONSOLE,
        b"T: a thread stays a zombie until its last handle closes\n",
    );
    let t = thread(last, base + 2 * 4096, 0, 0);
    check(t >= 0, b"T: the main thread exits first\n");
    LAST.store(t as u64, Relaxed);
    thread_exit(0)
}
