//! `test=shell`'s init: a native shell. Reads a command line from the console; runs the builtins `cd`, `pwd`, `exit`
//! and `help` itself and anything else as a program from the boot archive, never from disk. It resolves each path
//! argument against its root handle and current directory and passes the program only a handle to what it needs,
//! narrowed by `dup` (`crates/user/CLAUDE.md`). Prints `msh: <command>: <error>` when a command fails.
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
extern "C" fn _start() -> ! {
    let mut cwd = Cwd {
        path: [0; PATH],
        len: 0,
    };
    let mut buf = [0; 256];
    loop {
        write(CONSOLE, b"msh> ");
        let len = read(CONSOLE, &mut buf).max(0) as usize;
        let mut words: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
        let mut count = 0;
        for word in buf[..len].split(|&b| b == b' ' || b == b'\n') {
            if !word.is_empty() && count < MAX_ARGS {
                words[count] = word;
                count += 1;
            }
        }
        let words = &words[..count];
        let Some(&command) = words.first() else {
            continue;
        };
        let result = match command {
            b"cd" => cd(&mut cwd, arg(words, 1)),
            b"pwd" => {
                write(CONSOLE, b"/");
                write(CONSOLE, &cwd.path[..cwd.len]);
                write(CONSOLE, b"\n");
                0
            }
            b"exit" => exit(0),
            b"help" => help(),
            _ => run(&cwd, words),
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

/// A new handle with only `rights` (and transfer) to the directory or file at `path` below the root.
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
    write(CONSOLE, b"builtins: cd pwd exit help\nprograms:\n");
    let mut buf = [0; 512];
    let mut start = 0;
    loop {
        let n = readdir(ARCHIVE, &mut buf, start);
        if n <= 0 {
            return n;
        }
        let entries = &buf[..n as usize];
        write(CONSOLE, entries);
        start += entries.iter().filter(|&&b| b == b'\n').count() as u64;
    }
}

/// Runs the archive's program `words[0]` with the console as handle 0, then the handles its job needs, and waits for
/// it; its exit code is an errno.
fn run(cwd: &Cwd, words: &[&[u8]]) -> i64 {
    let exe = open(ARCHIVE, words[0], 0);
    if exe == ENOENT {
        write(CONSOLE, b"msh: ");
        write(CONSOLE, words[0]);
        write(CONSOLE, b": command not found\n");
        return 0;
    }
    if exe < 0 {
        return exe;
    }
    let (mut handles, mut granted, mut args) = ([0; 3], 0, [0; PATH]);
    let result = grant(cwd, words, (&mut handles, &mut granted), &mut args).and_then(|len| {
        let process = spawn_at(
            exe as u64,
            &handles[..granted],
            CHILD_BUDGET,
            u64::MAX,
            &args[..len],
        );
        if process < 0 {
            return Err(process);
        }
        granted = 0;
        let code = wait(process as u64);
        close(process as u64);
        Ok(-code)
    });
    handles[..granted].iter().for_each(|&h| _ = close(h));
    close(exe as u64);
    result.unwrap_or_else(|error| error)
}

/// Puts the console and what `words[0]` needs in `handles` (counting them in `granted`), and its arguments in `args`:
/// every word, with the paths it changes replaced by their last component; returns the arguments' length.
fn grant(
    cwd: &Cwd,
    words: &[&[u8]],
    (handles, granted): (&mut [u64; 3], &mut usize),
    args: &mut [u8; PATH],
) -> Result<usize, i64> {
    let (rights, changes) = match words[0] {
        b"cat" | b"ls" => (READ, false),
        b"sync" => (WRITE, false),
        b"mkdir" | b"rm" | b"mv" => (WRITE, true),
        b"touch" | b"write" => (READ | WRITE, true),
        _ => (0, false),
    };
    let paths = match words[0] {
        b"cat" | b"ls" | b"sync" | b"mkdir" | b"rm" | b"touch" | b"write" => 1,
        b"mv" => 2,
        _ => 0,
    };
    let console = dup(CONSOLE, WRITE | TRANSFER);
    if console < 0 {
        return Err(console);
    }
    (handles[0], *granted) = (console as u64, 1);
    let mut argv: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
    argv[..words.len()].copy_from_slice(words);
    let mut outs = [[0; PATH]; 2];
    for (i, out) in outs.iter_mut().enumerate().take(paths) {
        let path = match (words[0], changes) {
            (b"sync", _) => &[][..],
            (_, true) => resolve(cwd, words.get(i + 1).ok_or(EINVAL)?, out)?,
            _ => resolve(cwd, arg(words, i + 1), out)?,
        };
        let dir = match changes {
            true => {
                let (dir, name) = split(path);
                argv[i + 1] = name;
                dir
            }
            false => path,
        };
        let handle = handle(dir, rights);
        if handle < 0 {
            return Err(handle);
        }
        handles[i + 1] = handle as u64;
        *granted += 1;
    }
    let mut len = 0;
    for word in &argv[..words.len()] {
        let end = len + word.len() + 1;
        args.get_mut(len..end - 1)
            .ok_or(E2BIG)?
            .copy_from_slice(word);
        len = end;
    }
    Ok(len)
}
