//! `test=fuzz`'s init: makes seeded random syscalls (every number, unknown ones too) with boundary and random
//! arguments, live, closed and narrowed handles, and bad spawns; exits 1 at the first result that is neither a count nor
//! a known errno, printing the call. Arguments: seed, calls, and the call from which to print each one before making
//! it. Skips only the calls that would end it (`exit`, `thread`, `thread_exit`, `kill` of itself), block it forever
//! (a read of an empty pipe, a write that does not fit, a spawn of anything but `nop`), or write over its own image and
//! stack; a console write that would print is cut to 0 bytes. Prints `fuzz: seed <seed>: <calls> calls ok, <n> skipped`.
#![no_std]
#![no_main]

use user::*;

/// init's handles: itself (kill right), the boot archive, the MogFS root.
const SELF: u64 = 1;
const ARCHIVE: u64 = 2;
const ROOT: u64 = 3;
const USER_BASE: u64 = 1 << 32;
/// The program image and its stack end here (the board's `USER_STACK_TOP`); `map`s follow.
const STACK_TOP: u64 = USER_BASE + (2 << 20);
const USER_END: u64 = 1 << 39;
const PAGE: u64 = 4096;
const SCRATCH: usize = 4 * PAGE as usize;
/// The kernel's longest user buffer and `map`.
const MAX_BUFFER: u64 = 4096;
const MAX_MAP: u64 = 16 * PAGE;
/// Bytes mapped above `STACK_TOP`, the scratch memory included, after which `map` gets only bad lengths.
const MAPS: u64 = 32 * PAGE;
/// The socket calls (20-25) see no NetStack under `test=fuzz`, so they only reach their handle checks.
const LAST_SYSCALL: u64 = 25;
const POOL: usize = 48;

const ERRNOS: [i64; 25] = [
    EPERM,
    ENOENT,
    EIO,
    E2BIG,
    ENOEXEC,
    EBADF,
    EAGAIN,
    ENOMEM,
    EACCES,
    EFAULT,
    EBUSY,
    EEXIST,
    ENOTDIR,
    EISDIR,
    EINVAL,
    ENFILE,
    EMFILE,
    EFBIG,
    ENOSPC,
    EROFS,
    EPIPE,
    EDEADLK,
    ENAMETOOLONG,
    ENOSYS,
    ENOTEMPTY,
];

/// The only paths opened under the boot archive, so the fuzzer never spawns a program that blocks: `nop` exits at
/// once, `bad` is not an ELF.
const EXES: [&[u8]; 4] = [b"nop", b"bad", b"nope", b""];

/// Paths for `open`, `mkdir`, `unlink`, `rename` under MogFS directories.
const PATHS: [&[u8]; 16] = [
    b"a",
    b"b",
    b"a/b",
    b"a/c",
    b"b/a",
    b"..",
    b".",
    b"",
    b"/a",
    b"a//b",
    b"a/",
    b"a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a",
    b"a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a/a",
    b"nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    b"nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    b"x\0y",
];

/// `spawn` argument lists: valid (weighted), none, unterminated, empty strings, 33 arguments.
const SPAWN_ARGS: [&[u8]; 7] = [
    b"nop\0a\0",
    b"nop\0a\0",
    b"",
    b"nop",
    b"\0\0\0",
    b"",
    b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
];

/// What a live handle reaches, as far as the fuzzer needs to know.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Console,
    Archive,
    /// A MogFS directory or file.
    Fs,
    /// `nop` or `bad` from the boot archive.
    Exe,
    Process,
    Mutex,
    PipeRead(u64),
    PipeWrite(u64),
    Other,
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

struct Fuzzer {
    rng: Rng,
    /// Handle values the kernel returned, live or not; a closed value never names another handle (generations).
    pool: [(u64, Kind, bool); POOL],
    len: usize,
    /// Bytes buffered in the pipes this process made last, by their read end's first value; an untracked pipe is
    /// neither read nor written.
    pipes: [(u64, u64); 16],
    next_pipe: usize,
    scratch: &'static mut [u8],
    /// End of the mapped memory above `STACK_TOP`.
    mapped: u64,
}

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let number = |i: usize, default: u64| match args.get(i) {
        Some(arg) => arg.iter().fold(0u64, |n, &d| {
            n.wrapping_mul(10).wrapping_add(d.wrapping_sub(b'0') as u64)
        }),
        None => default,
    };
    let (seed, calls, trace) = (number(1, 1), number(2, 3000), number(3, u64::MAX));
    // A console without read (a read would wait for a line forever), and one to report on that no call can move.
    let console = dup(CONSOLE, WRITE | DUPLICATE | TRANSFER) as u64;
    let report = dup(CONSOLE, WRITE) as u64;
    let root = dup(ROOT, READ | WRITE | DUPLICATE) as u64;
    close(CONSOLE);
    let Some(scratch) = map(SCRATCH) else {
        return 1;
    };
    let mapped = scratch.as_mut_ptr() as u64 + SCRATCH as u64;
    let mut f = Fuzzer {
        rng: Rng(seed),
        pool: [(0, Kind::Other, false); POOL],
        len: 0,
        pipes: [(0, 0); 16],
        next_pipe: 0,
        scratch,
        mapped,
    };
    f.add(console, Kind::Console);
    f.add(report, Kind::Console);
    f.add(SELF, Kind::Other);
    f.add(ARCHIVE, Kind::Archive);
    f.add(ROOT, Kind::Fs);
    f.add(root, Kind::Fs);
    let nop = open(ARCHIVE, b"nop", 0) as u64;
    f.add(nop, Kind::Exe);
    f.add(open(ARCHIVE, b"bad", 0) as u64, Kind::Exe);
    // Never closed, so the fuzz keeps reaching its console, its root, the archive and a program to spawn.
    let kept = [report, root, ARCHIVE, nop];
    let mut skipped = 0;
    for call in 0..calls {
        let (nr, args, list) = loop {
            let (nr, mut args, list) = f.generate();
            if f.safe(nr, &mut args, &kept) {
                break (nr, args, list);
            }
            skipped += 1;
        };
        // Before the call, so a call that crashes the kernel still shows.
        if call >= trace {
            print(report, call, nr, &args);
        }
        // SAFETY: `safe` sends every buffer the kernel may write to the mapped memory above `STACK_TOP`, which only
        // `scratch` references, and that is not in use during the call.
        let (result, x1) = unsafe { raw(nr, args) };
        let ok = valid(nr, &args, result);
        if call < trace && !ok {
            print(report, call, nr, &args);
        }
        if call >= trace || !ok {
            write(report, if result < 0 { b" = -" } else { b" = " });
            write_u64(report, result.unsigned_abs());
            write(report, if ok { b"\n" } else { b": bad result\n" });
        }
        if !ok {
            return 1;
        }
        f.record(nr, &args, (result, x1), list);
    }
    write(report, b"fuzz: seed ");
    write_u64(report, seed);
    write(report, b": ");
    write_u64(report, calls);
    write(report, b" calls ok, ");
    write_u64(report, skipped);
    write(report, b" skipped\n");
    0
}

/// Whether `result` is a count or a known errno: `ENOSYS` alone for an unknown call, at most the length asked for
/// from `io_submit_wait` and `readdir`.
fn valid(nr: u64, args: &[u64; 7], result: i64) -> bool {
    if result < 0 {
        return ERRNOS.contains(&result) && (nr <= LAST_SYSCALL || result == ENOSYS);
    }
    match nr {
        1 => result as u64 <= args[3].min(MAX_BUFFER),
        14 => result as u64 <= args[2],
        _ => nr <= LAST_SYSCALL,
    }
}

/// Prints `fuzz: call <call>: <nr> <args>`, without a newline.
fn print(report: u64, call: u64, nr: u64, args: &[u64; 7]) {
    write(report, b"fuzz: call ");
    write_u64(report, call);
    write(report, b": ");
    write_u64(report, nr);
    for &arg in args {
        write(report, b" ");
        write_u64(report, arg);
    }
}

impl Fuzzer {
    /// Adds a live handle; once the pool is full, it replaces a closed one or one whose kind `safe` never checks.
    fn add(&mut self, value: u64, kind: Kind) {
        let pool = &self.pool[..self.len];
        let checked = |k| matches!(k, Kind::Console | Kind::PipeRead(_) | Kind::PipeWrite(_));
        let i = match pool.iter().position(|e| e.0 == value) {
            Some(i) => i,
            None if self.len < POOL => {
                self.len += 1;
                self.len - 1
            }
            None => pool
                .iter()
                .position(|e| !e.2)
                .or_else(|| pool.iter().position(|e| !checked(e.1)))
                .unwrap(),
        };
        self.pool[i] = (value, kind, true);
    }

    /// What `value` reaches, if it is a live handle.
    fn kind(&self, value: u64) -> Option<Kind> {
        let entry = self.pool[..self.len].iter().find(|e| e.0 == value && e.2)?;
        Some(entry.1)
    }

    fn drop(&mut self, value: u64) {
        if let Some(entry) = self.pool[..self.len].iter_mut().find(|e| e.0 == value) {
            entry.2 = false;
        }
    }

    fn buffered(&mut self, id: u64) -> Option<&mut u64> {
        Some(&mut self.pipes.iter_mut().find(|p| p.0 == id)?.1)
    }

    /// Mostly a live handle `want` accepts, else `handle`.
    fn handle_of(&mut self, want: fn(Kind) -> bool) -> u64 {
        let count = self.pool[..self.len]
            .iter()
            .filter(|e| e.2 && want(e.1))
            .count() as u64;
        if count == 0 || self.rng.below(4) == 0 {
            return self.handle();
        }
        let i = self.rng.below(count) as usize;
        let mut matching = self.pool[..self.len].iter().filter(|e| e.2 && want(e.1));
        matching.nth(i).unwrap().0
    }

    /// Mostly a live handle, else a closed one, a boundary value or anything.
    fn handle(&mut self) -> u64 {
        let live = self.pool[..self.len].iter().filter(|e| e.2).count() as u64;
        match self.rng.below(10) {
            0..=5 if live > 0 => {
                let i = self.rng.below(live) as usize;
                self.pool[..self.len]
                    .iter()
                    .filter(|e| e.2)
                    .nth(i)
                    .unwrap()
                    .0
            }
            0..=6 => self.pool[self.rng.below(self.len as u64) as usize].0,
            7 => self
                .rng
                .pick(&[0, 1, 2, 3, 16, u32::MAX as u64, 1 << 32, 1 << 63, u64::MAX]),
            8 => self.rng.below(16),
            _ => self.rng.next(),
        }
    }

    fn len(&mut self) -> u64 {
        match self.rng.below(8) {
            0 => self
                .rng
                .pick(&[0, 1, 7, 4095, 4096, 4097, 1 << 32, 1 << 63, u64::MAX]),
            1 => self.rng.next(),
            2 => self.rng.below(8192),
            _ => self.rng.below(256),
        }
    }

    /// A pointer for `len` bytes: into the scratch memory (unaligned too, or straddling its end), the program, the
    /// stack, the kernel, past user space or across its end, or anywhere.
    fn ptr(&mut self, len: u64) -> u64 {
        let base = self.scratch.as_mut_ptr() as u64;
        match self.rng.below(12) {
            0..=4 => base + self.rng.below(SCRATCH as u64),
            5 => self.mapped - self.rng.below(len.clamp(1, 64)),
            6 => self.rng.pick(&[
                0,
                u64::MAX,
                0x0900_0000,
                0x4000_0000,
                0x4020_0000,
                USER_BASE - 1,
                1 << 48,
                0xffff_0000_0000_0000,
            ]),
            7 => self
                .rng
                .pick(&[USER_BASE, USER_BASE + 1, STACK_TOP - 8, STACK_TOP + 3]),
            8 => USER_END - self.rng.below(len.clamp(1, 8192)),
            9 => USER_BASE + self.rng.below(USER_END - USER_BASE),
            _ => self.rng.next(),
        }
    }

    /// A path: from `PATHS`, copied to a random place in the scratch memory, or random memory.
    fn path(&mut self) -> (u64, u64) {
        if self.rng.below(4) == 0 {
            let len = self.len();
            return (self.ptr(len), len);
        }
        self.listed(&PATHS, PAGE as usize)
    }

    /// One of `items`, copied into the scratch memory at `area` plus a random offset; its address and length.
    fn listed(&mut self, items: &[&[u8]], area: usize) -> (u64, u64) {
        let item = self.rng.pick(items);
        let at = area + self.rng.below(PAGE - 64) as usize;
        self.scratch[at..at + item.len()].copy_from_slice(item);
        (self.scratch[at..].as_mut_ptr() as u64, item.len() as u64)
    }

    /// A syscall number and arguments, and how many handles a spawn lists from the scratch memory.
    fn generate(&mut self) -> (u64, [u64; 7], usize) {
        let mut a = [0; 7];
        // `close` twice as often, so the handle table does not stay full.
        let nr = match self.rng.below(21) {
            20 => 3,
            18 | 19 => {
                let any = self.rng.next();
                self.rng.pick(&[
                    20,
                    21,
                    23,
                    24,
                    26,
                    64,
                    255,
                    1 << 32 | 1,
                    1 << 32 | 6,
                    u64::MAX,
                    any,
                ])
            }
            nr => nr.max(1),
        };
        if self.rng.below(16) == 0 {
            for arg in &mut a {
                *arg = match self.rng.below(3) {
                    0 => self.handle(),
                    1 => self.len(),
                    _ => self.rng.next(),
                };
            }
            return (nr, a, 0);
        }
        let mut list = 0;
        match nr {
            1 => {
                a[0] = self.handle_of(|k| {
                    matches!(
                        k,
                        Kind::Console | Kind::Fs | Kind::PipeRead(_) | Kind::PipeWrite(_)
                    )
                });
                a[1] = self.rng.pick(&[0, 1, 0, 1, 2, u64::MAX]);
                a[3] = self.len();
                a[2] = self.ptr(a[3]);
                let any = self.rng.below(60000);
                a[4] = self
                    .rng
                    .pick(&[0, 1, 4087, 4088, 57231, 57232, 1 << 32, u64::MAX, any]);
            }
            2 => {
                a[0] = self.handle();
                a[1] = match self.rng.below(4) {
                    0 => self.rng.next(),
                    _ => self.rng.below(256),
                };
            }
            3 => a[0] = self.handle(),
            8 | 12 => a[0] = self.handle_of(|k| k == Kind::Process),
            10 | 11 => a[0] = self.handle_of(|k| k == Kind::Mutex),
            15 => a[0] = self.handle_of(|k| k == Kind::Fs),
            4 => {
                let any = self.rng.below(MAX_MAP);
                a[0] = self
                    .rng
                    .pick(&[0, 1, PAGE, PAGE + 1, MAX_MAP, MAX_MAP + 1, u64::MAX, any]);
            }
            5 | 13 | 16 => {
                a[0] = self.handle_of(|k| matches!(k, Kind::Fs | Kind::Archive));
                (a[1], a[2]) = self.path();
                a[3] = self
                    .rng
                    .pick(&[0, 0, CREATE, TRUNC, CREATE | TRUNC, 4, u64::MAX]);
            }
            17 => {
                a[0] = self.handle_of(|k| k == Kind::Fs);
                (a[1], a[2]) = self.path();
                a[3] = self.handle_of(|k| k == Kind::Fs);
                (a[4], a[5]) = self.path();
            }
            14 => {
                a[0] = self.handle_of(|k| matches!(k, Kind::Fs | Kind::Archive));
                a[2] = self.len();
                a[1] = self.ptr(a[2]);
                a[3] = self.rng.pick(&[0, 0, 1, 2, 64, 1 << 32, u64::MAX]);
            }
            6 => {
                a[0] = self.handle_of(|k| k == Kind::Exe);
                list = self.rng.pick(&[0, 0, 0, 1, 2, 16, 17]);
                let at = 2 * PAGE as usize + self.rng.below(64) as usize;
                for i in 0..list {
                    let handle = self.handle();
                    self.scratch[at + 8 * i..at + 8 * i + 8].copy_from_slice(&handle.to_le_bytes());
                }
                (a[1], a[2]) = (self.scratch[at..].as_mut_ptr() as u64, list as u64);
                if self.rng.below(5) == 0 {
                    (a[2], list) = (self.len(), 0);
                    a[1] = self.ptr(a[2].saturating_mul(8));
                }
                let any = self.rng.below(32);
                a[3] = self
                    .rng
                    .pick(&[10, 10, 12, 0, 9, 64, 1 << 20, u64::MAX, any]);
                a[4] = self.rng.pick(&[0, 1, 255, 256, u64::MAX]);
                (a[5], a[6]) = match self.rng.below(5) {
                    0 => {
                        let len = self.len();
                        (self.ptr(len), len)
                    }
                    _ => self.listed(&SPAWN_ARGS, 3 * PAGE as usize),
                };
            }
            _ => {}
        }
        (nr, a, list)
    }

    /// Whether the call may be made; rewrites a console write to 0 bytes unless it faults, so nothing reaches the
    /// serial. An `open` under the boot archive takes a path from `EXES`.
    fn safe(&mut self, nr: u64, a: &mut [u64; 7], kept: &[u64]) -> bool {
        // Writing `len` bytes at `ptr` must stay off the program, its data and its stack.
        let off_image = |ptr: u64, len: u64| {
            len == 0 || ptr >= STACK_TOP || ptr.saturating_add(len) <= USER_BASE
        };
        match nr {
            // `exit`, `thread_exit` of its only thread, and a `thread` at a random entry, which faults, end it.
            0 | 18 | 19 => false,
            1 => {
                let len = a[3].min(MAX_BUFFER);
                match (self.kind(a[0]), a[1]) {
                    (Some(Kind::PipeRead(id)), 0) if self.buffered(id).is_none_or(|b| *b == 0) => {
                        return false;
                    }
                    (Some(Kind::PipeWrite(id)), 1)
                        if self.buffered(id).is_none_or(|b| *b + len > MAX_BUFFER) =>
                    {
                        return false;
                    }
                    (Some(Kind::Console), 1)
                        if !(a[2] >= self.mapped || a[2].saturating_add(len) <= USER_BASE) =>
                    {
                        a[3] = 0
                    }
                    _ => {}
                }
                a[1] != 0 || off_image(a[2], len)
            }
            3 => !kept.contains(&a[0]),
            5 => {
                if a[0] == ARCHIVE {
                    (a[1], a[2]) = self.listed(&EXES, PAGE as usize);
                }
                true
            }
            // Mapped memory is never returned: past `MAPS`, only lengths `map` rejects, so the budget lasts.
            4 => self.mapped - STACK_TOP < MAPS || a[0] == 0 || a[0] > MAX_MAP,
            12 => a[0] != SELF,
            14 => a[2] > MAX_BUFFER || off_image(a[1], a[2]),
            _ => true,
        }
    }

    /// Tracks the handles, pipe contents and mapped memory the call changed.
    fn record(&mut self, nr: u64, a: &[u64; 7], (result, x1): (i64, u64), list: usize) {
        // A handle a spawn moved without the fuzzer knowing shows as closed here; not through `wait` or `kill`, whose
        // EBADF can also mean a live handle to a reaped child whose slot was reused.
        if result == EBADF && matches!(nr, 1..=3 | 5 | 10 | 11 | 13..=16) {
            self.drop(a[0]);
        }
        if result < 0 {
            return;
        }
        match nr {
            1 => match (self.kind(a[0]), a[1]) {
                (Some(Kind::PipeRead(id)), 0) => *self.buffered(id).unwrap() -= result as u64,
                (Some(Kind::PipeWrite(id)), 1) => *self.buffered(id).unwrap() += result as u64,
                _ => {}
            },
            2 => {
                let kind = self.kind(a[0]).unwrap_or(Kind::Other);
                self.add(result as u64, kind);
            }
            3 => self.drop(a[0]),
            4 => self.mapped = self.mapped.max(result as u64 + a[0].div_ceil(PAGE) * PAGE),
            5 if a[0] == ARCHIVE => self.add(result as u64, Kind::Exe),
            5 => self.add(result as u64, Kind::Fs),
            9 => self.add(result as u64, Kind::Mutex),
            6 => {
                for i in 0..list {
                    let at = a[1] as usize - self.scratch.as_mut_ptr() as usize + 8 * i;
                    let handle = u64::from_le_bytes(self.scratch[at..at + 8].try_into().unwrap());
                    self.drop(handle);
                }
                self.add(result as u64, Kind::Process);
            }
            7 => {
                let id = result as u64;
                self.pipes[self.next_pipe] = (id, 0);
                self.next_pipe = (self.next_pipe + 1) % self.pipes.len();
                self.add(id, Kind::PipeRead(id));
                self.add(x1, Kind::PipeWrite(id));
            }
            _ => {}
        }
    }
}
