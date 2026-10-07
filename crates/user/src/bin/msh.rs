//! `test=shell`'s init: a native shell. Reads a command line from the console; runs the builtins `cd`, `pwd`, `exit`
//! and `help` itself and the commands in `user::COMMANDS` as programs from the boot archive, never from disk. It
//! resolves each path argument against its root handle and current directory and passes the program only a handle to
//! what it needs, narrowed by `dup` (`crates/user/CLAUDE.md`). Prints `msh: <command>: <error>` when a command fails.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const ARCHIVE: u64 = 2;
/// init's root directory handle, present when a disk is mounted.
const ROOT: u64 = 3;
/// A command's budget: a shell program's 10 frames (3 tables, text, 2 stack, 4 kernel stack) and room for two more
/// pages of program; msh's own 11 (3 pages of program) leave 14 of its 25.
const CHILD_BUDGET: usize = 12;
/// A `Grant::Posix` program's budget: busybox sh with its 128 KiB stack and heap, and the children it spawns (libc
/// gives each up to 1024 frames, halving on `ENOMEM`).
const POSIX_BUDGET: usize = 2048;
/// Longest path msh keeps or builds, and its argument buffer (a command line is at most 256 bytes).
const PATH: usize = 256;

const ERRORS: [(i64, &[u8]); 18] = [
    (ENOENT, b"ENOENT"),
    (EIO, b"EIO"),
    (E2BIG, b"E2BIG"),
    (ENOEXEC, b"ENOEXEC"),
    (EBADF, b"EBADF"),
    (ENOMEM, b"ENOMEM"),
    (EACCES, b"EACCES"),
    (EFAULT, b"EFAULT"),
    (EBUSY, b"EBUSY"),
    (EEXIST, b"EEXIST"),
    (ENOTDIR, b"ENOTDIR"),
    (EISDIR, b"EISDIR"),
    (EINVAL, b"EINVAL"),
    (EMFILE, b"EMFILE"),
    (EFBIG, b"EFBIG"),
    (ENOSPC, b"ENOSPC"),
    (EROFS, b"EROFS"),
    (ENOTEMPTY, b"ENOTEMPTY"),
];

/// The current directory: a path below the root, empty for the root itself.
struct Cwd {
    path: [u8; PATH],
    len: usize,
}

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: `argc` and `len` are the x0 and x2 this process started with.
    unsafe { start(argc, len, main) }
}

/// Without arguments (boot), reads command lines from the console. With them (`test=bench-shell`), runs each argument
/// as a command line and prints `bench <line>: <ns> ns`, the time from spawning its program until reaping it.
fn main(args: &[&[u8]]) -> u64 {
    let mut cwd = Cwd {
        path: [0; PATH],
        len: 0,
    };
    if let [_, lines @ ..] = args {
        let per_s = ticks_per_s() as u128;
        for line in lines {
            let mut spent = 0;
            command(&mut cwd, line, &mut spent);
            write(CONSOLE, b"bench ");
            write(CONSOLE, line);
            write(CONSOLE, b": ");
            write_u64(CONSOLE, (spent as u128 * 1_000_000_000 / per_s) as u64);
            write(CONSOLE, b" ns\n");
        }
        return 0;
    }
    let mut buf = [0; 256];
    loop {
        write(CONSOLE, b"msh> ");
        let len = read(CONSOLE, &mut buf).max(0) as usize;
        command(&mut cwd, &buf[..len], &mut 0);
    }
}

/// Runs the command `line`, adding the counter ticks from spawning its program until reaping it to `spent`.
fn command(cwd: &mut Cwd, line: &[u8], spent: &mut u64) {
    let mut words: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
    let mut count = 0;
    let mut rest = line;
    while count < MAX_ARGS {
        let start = rest.iter().position(|&b| b != b' ' && b != b'\n');
        let Some(start) = start else { break };
        rest = &rest[start..];
        // A word in single quotes keeps its spaces.
        let (word, end) = match rest[0] {
            b'\'' => match rest[1..].iter().position(|&b| b == b'\'') {
                Some(i) => (&rest[1..=i], i + 2),
                None => (&rest[1..], rest.len()),
            },
            _ => {
                let i = rest.iter().position(|&b| b == b' ' || b == b'\n');
                let i = i.unwrap_or(rest.len());
                (&rest[..i], i)
            }
        };
        words[count] = word;
        count += 1;
        rest = &rest[end..];
    }
    let words = &words[..count];
    let Some(&command) = words.first() else {
        return;
    };
    let result = match command {
        b"cd" => cd(cwd, arg(words, 1)),
        b"pwd" => {
            write(CONSOLE, b"/");
            write(CONSOLE, &cwd.path[..cwd.len]);
            write(CONSOLE, b"\n");
            0
        }
        b"exit" => exit(0),
        b"help" => help(),
        _ => run(cwd, words, spent),
    };
    if result < 0 {
        write(CONSOLE, b"msh: ");
        write(CONSOLE, command);
        write(CONSOLE, b": ");
        match ERRORS.iter().find(|e| e.0 == result) {
            Some((_, name)) => {
                write(CONSOLE, name);
            }
            None => write_u64(CONSOLE, result.unsigned_abs()),
        }
        write(CONSOLE, b"\n");
    }
}

/// `path` relative to `cwd`, as a path below the root, in `out`.
fn resolve<'a>(cwd: &Cwd, path: &[u8], out: &'a mut [u8; PATH]) -> Result<&'a [u8], i64> {
    let parts: [&[u8]; 3] = match (cwd.len, path.len()) {
        (0, _) => [path, b"", b""],
        (_, 0) => [&cwd.path[..cwd.len], b"", b""],
        _ => [&cwd.path[..cwd.len], b"/", path],
    };
    let mut len = 0;
    for part in parts {
        out.get_mut(len..len + part.len())
            .ok_or(EINVAL)?
            .copy_from_slice(part);
        len += part.len();
    }
    Ok(&out[..len])
}

/// A new handle with only `rights` (and transfer) to the directory or file at `path` below the root (the root if empty).
fn handle(path: &[u8], rights: u64) -> i64 {
    if path.is_empty() {
        return dup(ROOT, rights | TRANSFER);
    }
    let wide = open(ROOT, path, 0);
    if wide < 0 {
        return wide;
    }
    let narrow = dup(wide as u64, rights | TRANSFER);
    close(wide as u64);
    narrow
}

/// `path`'s parent directory and last component.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (b"", path),
    }
}

fn cd(cwd: &mut Cwd, path: &[u8]) -> i64 {
    if path == b".." {
        cwd.len = split(&cwd.path[..cwd.len]).0.len();
        return 0;
    }
    let mut out = [0; PATH];
    let path = match path {
        b"" => &[][..],
        path => match resolve(cwd, path, &mut out) {
            Ok(path) => path,
            Err(error) => return error,
        },
    };
    if !path.is_empty() {
        let dir = handle(path, READ);
        if dir < 0 {
            return dir;
        }
        // Only a directory lists; from past its end, it reads nothing.
        let listed = readdir(dir as u64, &mut [], u64::MAX);
        close(dir as u64);
        if listed < 0 {
            return listed;
        }
    }
    cwd.path[..path.len()].copy_from_slice(path);
    cwd.len = path.len();
    0
}

fn help() -> i64 {
    write(CONSOLE, b"builtins: cd pwd exit help\ncommands:");
    for (name, _) in COMMANDS {
        write(CONSOLE, b" ");
        write(CONSOLE, name);
    }
    write(CONSOLE, b"\n");
    0
}

/// Runs the command `words[0]` from msh's table, a program in the boot archive, with the console as handle 0 and then
/// the handles its `Grant` names, and waits for it, adding the ticks from its spawn to its reaping to `spent`; its exit
/// code is an errno.
fn run(cwd: &Cwd, words: &[&[u8]], spent: &mut u64) -> i64 {
    let Some(grant) = grant(words[0]) else {
        write(CONSOLE, b"msh: ");
        write(CONSOLE, words[0]);
        write(CONSOLE, b": command not found\n");
        return 0;
    };
    let exe = open(ARCHIVE, words[0], 0);
    if exe < 0 {
        return exe;
    }
    let budget = match grant {
        Grant::Posix => POSIX_BUDGET,
        _ => CHILD_BUDGET,
    };
    let (mut handles, mut granted, mut args) = ([0; 5], 0, [0; PATH]);
    let result = give(cwd, words, grant, (&mut handles, &mut granted), &mut args).and_then(|len| {
        let start = ticks();
        let process = spawn_at(
            exe as u64,
            &handles[..granted],
            budget,
            u64::MAX,
            &args[..len],
        );
        if process < 0 {
            return Err(process);
        }
        granted = 0;
        let code = wait(process as u64);
        *spent += ticks() - start;
        close(process as u64);
        Ok(-code)
    });
    handles[..granted].iter().for_each(|&h| _ = close(h));
    close(exe as u64);
    result.unwrap_or_else(|error| error)
}

/// Puts the console (write only) and what `grant` names in `handles` (counting them in `granted`), and the arguments
/// in `args`: every word, the paths a `Grant::Parents` changes replaced by their last component; returns their length.
fn give(
    cwd: &Cwd,
    words: &[&[u8]],
    grant: Grant,
    (handles, granted): (&mut [u64; 5], &mut usize),
    args: &mut [u8; PATH],
) -> Result<usize, i64> {
    if grant == Grant::Posix {
        return posix(cwd, words, (handles, granted), args);
    }
    let console = dup(CONSOLE, WRITE | TRANSFER);
    if console < 0 {
        return Err(console);
    }
    (handles[0], *granted) = (console as u64, 1);
    let mut argv: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
    argv[..words.len()].copy_from_slice(words);
    let mut outs = [[0; PATH]; 2];
    let [first, second] = &mut outs;
    let mut grants: [(&[u8], u64); 2] = [(b"", 0); 2];
    let count = match grant {
        Grant::Console | Grant::Posix => 0,
        Grant::Root(rights) => {
            grants[0] = (b"", rights);
            1
        }
        Grant::Target(rights) => {
            grants[0] = (resolve(cwd, arg(words, 1), first)?, rights);
            1
        }
        Grant::Parents(rights, n) => {
            for (i, out) in [first, second].into_iter().enumerate().take(n) {
                let (dir, name) = split(resolve(cwd, words.get(i + 1).ok_or(EINVAL)?, out)?);
                (grants[i], argv[i + 1]) = ((dir, rights), name);
            }
            n
        }
    };
    for &(path, rights) in &grants[..count] {
        let handle = handle(path, rights);
        if handle < 0 {
            return Err(handle);
        }
        handles[*granted] = handle as u64;
        *granted += 1;
    }
    let mut len = 0;
    for word in &argv[..words.len()] {
        push(args, &mut len, &[word])?;
    }
    Ok(len)
}

/// `Grant::Posix`: the console as stdin (read), stdout and stderr (write), the root and the archive, and the
/// arguments after `<argc> /<cwd>`, which tells musl's start code the current directory.
fn posix(
    cwd: &Cwd,
    words: &[&[u8]],
    (handles, granted): (&mut [u64; 5], &mut usize),
    args: &mut [u8; PATH],
) -> Result<usize, i64> {
    let grants = [
        (CONSOLE, READ),
        (CONSOLE, WRITE),
        (CONSOLE, WRITE),
        (ROOT, READ | WRITE),
        (ARCHIVE, READ | EXEC),
    ];
    for (handle, rights) in grants {
        let dup = dup(handle, rights | DUPLICATE | TRANSFER);
        if dup < 0 {
            return Err(dup);
        }
        handles[*granted] = dup as u64;
        *granted += 1;
    }
    let argc = words.len() as u8;
    let digits = [b'0' + argc / 10, b'0' + argc % 10];
    let digits = if argc < 10 { &digits[1..] } else { &digits[..] };
    let mut len = 0;
    push(args, &mut len, &[digits, b" /", &cwd.path[..cwd.len]])?;
    for word in words {
        push(args, &mut len, &[word])?;
    }
    Ok(len)
}

/// Appends `parts` and a NUL at `len` in `args`; `E2BIG` if they do not fit.
fn push(args: &mut [u8; PATH], len: &mut usize, parts: &[&[u8]]) -> Result<(), i64> {
    for part in parts {
        args.get_mut(*len..*len + part.len())
            .ok_or(E2BIG)?
            .copy_from_slice(part);
        *len += part.len();
    }
    *args.get_mut(*len).ok_or(E2BIG)? = 0;
    *len += 1;
    Ok(())
}
