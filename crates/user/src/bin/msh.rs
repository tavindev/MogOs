//! `test=shell`'s init: a native shell. Reads a command line from the console, runs it against the root directory,
//! and prints `msh: <command>: <error>` when it fails.
#![no_std]
#![no_main]

use user::*;

/// init's root directory handle, present when a disk is mounted.
const ROOT: u64 = 3;

const ERRORS: [(i64, &[u8]); 11] = [
    (ENOENT, b"ENOENT"),
    (EIO, b"EIO"),
    (EBADF, b"EBADF"),
    (EACCES, b"EACCES"),
    (EEXIST, b"EEXIST"),
    (ENOTDIR, b"ENOTDIR"),
    (EISDIR, b"EISDIR"),
    (EINVAL, b"EINVAL"),
    (EFBIG, b"EFBIG"),
    (ENOSPC, b"ENOSPC"),
    (EROFS, b"EROFS"),
];

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let mut buf = [0; 256];
    loop {
        write(CONSOLE, b"msh> ");
        let len = read(CONSOLE, &mut buf).max(0) as usize;
        let line = buf[..len].strip_suffix(b"\n").unwrap_or(&buf[..len]);
        let (command, args) = split(line);
        let (path, text) = split(args);
        let result = match command {
            b"" => continue,
            b"ls" => ls(path),
            b"mkdir" => mkdir(ROOT, path),
            b"touch" => with_file(path, CREATE, |_| 0),
            b"write" => with_file(path, CREATE | TRUNC, |file| {
                write_at(file, text, 0).min(write_at(file, b"\n", text.len() as u64))
            }),
            b"cat" => with_file(path, 0, cat),
            b"sync" => sync(ROOT),
            b"exit" => exit(0),
            _ => ENOENT,
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

/// The text before the first space and the text after it.
fn split(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b' ') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, &[]),
    }
}

/// Opens `path` with `flags`, runs `f` on it and closes it; the first error.
fn with_file(path: &[u8], flags: u64, f: impl FnOnce(u64) -> i64) -> i64 {
    let file = open(ROOT, path, flags);
    if file < 0 {
        return file;
    }
    let result = f(file as u64);
    close(file as u64);
    result
}

fn ls(path: &[u8]) -> i64 {
    match path {
        b"" => list(ROOT),
        path => with_file(path, 0, list),
    }
}

fn list(dir: u64) -> i64 {
    let mut buf = [0; 512];
    let mut start = 0;
    loop {
        let n = readdir(dir, &mut buf, start);
        if n <= 0 {
            return n;
        }
        let entries = &buf[..n as usize];
        write(CONSOLE, entries);
        start += entries.iter().filter(|&&b| b == b'\n').count() as u64;
    }
}

fn cat(file: u64) -> i64 {
    let mut buf = [0; 512];
    let mut offset = 0;
    loop {
        let n = read_at(file, &mut buf, offset);
        if n <= 0 {
            return n;
        }
        write(CONSOLE, &buf[..n as usize]);
        offset += n as u64;
    }
}
