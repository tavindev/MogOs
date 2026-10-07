//! Native syscalls: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

use mogfs::Inode;

use crate::handle::{EXEC, Handles, KILL as KILL_RIGHT, MAX_HANDLES, Object, READ, WRITE};
use crate::mutex::Mutex;
use crate::pipe::End;

/// `exit(code)`: ends the calling process; `wait` reports the low 8 bits of `code`.
const EXIT: u64 = 0;
/// `io_submit_wait(handle, op, ptr, len, offset)`: submits I/O on `handle` and waits for it to complete; returns the
/// bytes moved. `op` is `IO_READ` (into `ptr`, read right) or `IO_WRITE` (from `ptr`, write right). libc's `pread` and
/// `pwrite`; a file is read or written at `offset` (a read at or past its end returns 0), which the console and pipes
/// ignore. A `len` over `MAX_BUFFER` moves at most `MAX_BUFFER` bytes (a short read or write). Reading the console
/// waits for a line (`console::Line`); with two readers, whichever runs first gets it.
const IO: u64 = 1;
/// `dup(handle, rights)`: returns a new handle to the same object with `rights`, a subset of `handle`'s (duplicate right).
const DUP: u64 = 2;
/// `close(handle)`: returns 0.
const CLOSE: u64 = 3;
/// `map(len)`: maps `len` bytes (rounded up to pages) of zeroed read-write memory at an address the kernel picks,
/// charged to the caller's budget; returns the address. The kernel picking the address leaves nothing to overlap, and
/// it is what musl's `mmap(NULL, ...)` needs (its malloc falls back from `brk` to `mmap`).
const MAP: u64 = 4;
/// `open(dir, path_ptr, path_len, flags)`: returns a handle with `dir`'s rights to the file or directory at `path`
/// under the directory `dir` (read right). `flags`: `CREATE` makes a missing file, `TRUNC` empties the file; either
/// needs the write right, and on the boot archive is `EROFS`. Paths resolve only below a directory handle: each
/// `/`-separated component must be a name, so `..`, `.`, an empty component (`/x`, `a//b`) is `EINVAL`; more than
/// `file::MAX_DEPTH` (16) components is `ENAMETOOLONG`.
const OPEN: u64 = 5;
/// `spawn(exe, handles_ptr, handles_len, budget, priority)`: starts the executable `exe` (exec right) as a new process
/// at `priority`, capped at the caller's own (so no process escalates), moving it the `handles_len` handles at
/// `handles_ptr` (transfer right; values 0, 1, ... in the child) and `budget` frames of the caller's budget; returns a
/// handle to the process (wait, kill). On failure nothing moves.
const SPAWN: u64 = 6;
/// `pipe()`: returns a handle to a new pipe's read end (read, duplicate, transfer), and in `x1` one to its write end
/// (write, duplicate, transfer). Its one-page buffer is charged to the caller's budget until the last handle to it
/// closes. Reading it empty waits for data, or returns 0 once no write end is left; a write waits until all of it fits
/// (so every write is atomic, as `MAX_BUFFER` is the buffer size), or fails with `EPIPE` once no read end is left.
const PIPE: u64 = 7;
/// `wait(process)`: waits for the process (wait right) to exit; returns its exit code (`KILLED` if a fault killed it)
/// and moves what is left of its budget back to the caller. An exited process keeps its slot until waited for or its
/// handle closes; after that, `EBADF` once a newer process took the slot.
const WAIT: u64 = 8;
/// `mutex()`: returns a handle (duplicate, transfer) to a new unlocked mutex; the table slot is fixed, so nothing is
/// charged.
const MUTEX: u64 = 9;
/// `lock(mutex)`: waits until the mutex is free, then makes the caller its owner; meanwhile the owner runs at least
/// at the caller's priority. `EDEADLK` if the caller owns it. Lock and unlock need no right.
const LOCK: u64 = 10;
/// `unlock(mutex)`: frees the mutex, which the caller must own (`EPERM`). An exiting owner frees what it holds.
const UNLOCK: u64 = 11;
/// `kill(process)`: ends the process (kill right) as a fault would; `wait` reports `KILLED`. 0 if it already exited.
const KILL: u64 = 12;
/// `mkdir(dir, path_ptr, path_len)`: makes a directory at `path` under `dir` (write right), resolved as by `open`;
/// returns 0. `EROFS` on the boot archive.
const MKDIR: u64 = 13;
/// `readdir(dir, ptr, len, start)`: fills `ptr` with whole `name\n` entries (`name/\n` for a directory) of `dir` (read
/// right) from entry `start` on; returns the bytes written, 0 past the last entry. The caller advances `start` by the
/// newlines it got; an unlink between calls moves an entry, so a resumed listing can skip or repeat one. `EINVAL` if
/// the first entry does not fit in `len`.
const READDIR: u64 = 14;
/// `sync(handle)`: makes every change to the file system the directory or file `handle` (write right) is on durable, atomically; it holds the core
/// for its writes and two flushes. `EIO` means unknown: the changes may or may not be durable.
const SYNC: u64 = 15;
/// `unlink(dir, path_ptr, path_len)`: removes the file or empty directory (`ENOTEMPTY` otherwise) at `path` under `dir`
/// (write right), resolved as by `open`; returns 0. `EROFS` on the boot archive.
const UNLINK: u64 = 16;
/// `rename(from_dir, from_ptr, from_len, to_dir, to_ptr, to_len)`: moves the entry at the path `from` under `from_dir`
/// to the path `to` under `to_dir` (both write right), resolved as by `open`; returns 0. `EEXIST` if `to` exists,
/// `EINVAL` if a directory would move below itself, `EROFS` on the boot archive.
const RENAME: u64 = 17;

/// The exit code `wait` reports for a process a fault killed: outside `exit`'s 0..=255.
pub const KILLED: u64 = 256;

pub const IO_READ: u64 = 0;
pub const IO_WRITE: u64 = 1;

/// `open` flags.
pub const CREATE: u64 = 1 << 0;
pub const TRUNC: u64 = 1 << 1;

// Errors are negated musl errno values.

/// Unlocking a mutex the caller does not own.
pub const EPERM: i64 = -1;
/// No such file in the directory.
pub const ENOENT: i64 = -2;
/// The disk failed a request, or the file system is corrupt.
pub const EIO: i64 = -5;
/// Not a valid executable.
pub const ENOEXEC: i64 = -8;

/// Bad, closed or stale handle.
pub const EBADF: i64 = -9;
/// No free process slot.
pub const EAGAIN: i64 = -11;
/// Over the memory budget, or out of frames.
pub const ENOMEM: i64 = -12;
/// The handle lacks a right the call needs.
pub const EACCES: i64 = -13;
/// Bad address: outside user space, unmapped, or (except for `io_submit_wait`) longer than `MAX_BUFFER`.
pub const EFAULT: i64 = -14;
/// The name exists.
pub const EEXIST: i64 = -17;
/// A path component, or a handle a call needs to be a directory, is a file.
pub const ENOTDIR: i64 = -20;
/// Reading, writing or truncating a directory.
pub const EISDIR: i64 = -21;
/// Invalid argument: a `map` of zero bytes or more than `MAX_MAP`, a `spawn` of more than `MAX_HANDLES` handles, an
/// unknown I/O op or `open` flag, a path component that is not a name.
pub const EINVAL: i64 = -22;
/// The pipe or mutex table is full.
pub const ENFILE: i64 = -23;
/// The handle table is full.
pub const EMFILE: i64 = -24;
/// Past the largest file or directory.
pub const EFBIG: i64 = -27;
/// The file system is full.
pub const ENOSPC: i64 = -28;
/// Changing the boot archive.
pub const EROFS: i64 = -30;
/// Writing a pipe with no read end left.
pub const EPIPE: i64 = -32;
/// Locking a mutex the caller owns.
pub const EDEADLK: i64 = -35;
/// A path of more than `file::MAX_DEPTH` components.
pub const ENAMETOOLONG: i64 = -36;
/// No such syscall.
const ENOSYS: i64 = -38;
/// Unlinking a directory that has entries.
pub const ENOTEMPTY: i64 = -39;

/// User virtual addresses: 4 GiB up to the 39-bit VA limit.
const USER: Range<u64> = 1 << 32..1 << 39;
/// Longest user buffer a syscall reads or writes (I/O data, `open` name, `spawn` handles), so its IRQs-masked work
/// stays bounded.
const MAX_BUFFER: u64 = 4096;
const _: () = assert!(
    MAX_BUFFER as usize <= crate::pipe::SIZE,
    "a longer pipe write would never fit"
);
/// Longest `map` (16 pages), so its IRQs-masked zeroing stays bounded.
const MAX_MAP: u64 = 16 * 4096;

pub enum Call {
    /// End the caller with this code.
    Exit(u64),
    /// Write to the console; `ptr..ptr + len` lies in `USER` unless empty, but may be unmapped.
    Write {
        ptr: u64,
        len: usize,
    },
    /// Read a line from the console into `ptr..ptr + len`, as for `Write`.
    Read {
        ptr: u64,
        len: usize,
    },
    /// Read from (or, for a write end, write to) the pipe `end` reaches; `ptr..ptr + len` as for `Write`.
    Pipe {
        end: End,
        ptr: u64,
        len: usize,
    },
    /// Create a pipe.
    NewPipe,
    /// Wait for the process in `slot` with `generation`.
    Wait {
        slot: usize,
        generation: u64,
    },
    /// `handle` is a new handle to `object`.
    Dup {
        handle: u64,
        object: Object,
    },
    /// A handle to this object was closed.
    Close(Object),
    /// Map this many pages into the caller's address space.
    Map {
        pages: usize,
    },
    /// Read (or write) the file `inode` at `offset` into (from) `ptr..ptr + len`, as for `Write`.
    File {
        inode: Inode,
        write: bool,
        offset: u64,
        ptr: u64,
        len: usize,
    },
    /// Open the path at `ptr..ptr + len` (in `USER` unless empty, maybe unmapped) under `dir` (the boot archive, or a
    /// directory, then with `flags`) with `rights`.
    Open {
        dir: Object,
        ptr: u64,
        len: usize,
        flags: u64,
        rights: u64,
    },
    /// Make a directory at the path at `ptr..ptr + len` (as for `Open`) under `dir`.
    Mkdir {
        dir: Inode,
        ptr: u64,
        len: usize,
    },
    /// List `dir` (the boot archive or a directory) from entry `start` into `ptr..ptr + len`, as for `Read`.
    Readdir {
        dir: Object,
        ptr: u64,
        len: usize,
        start: u64,
    },
    /// Commit the file system.
    Sync,
    /// Unlink the path at `ptr..ptr + len` (as for `Open`) under `dir`.
    Unlink {
        dir: Inode,
        ptr: u64,
        len: usize,
    },
    /// Rename the path `from` to the path `to`, each a directory and a buffer as for `Unlink`.
    Rename {
        from: (Inode, u64, usize),
        to: (Inode, u64, usize),
    },
    /// Spawn the boot archive's file `file`, moving the `len` handles at `ptr` (in `USER` unless empty, maybe unmapped) and `budget`.
    Spawn {
        file: Range<usize>,
        ptr: u64,
        len: usize,
        budget: usize,
        priority: u64,
    },
    /// Create a mutex.
    NewMutex,
    Lock(Mutex),
    Unlock(Mutex),
    /// Kill the process in `slot` with `generation`.
    Kill {
        slot: usize,
        generation: u64,
    },
}

/// Runs syscall `nr` with arguments `args` (`x0`-`x5`) against the caller's `handles`, leaving the board the parts
/// that touch hardware or tasks; `Err` holds the result to return.
pub fn dispatch(nr: u64, args: &[u64; 6], handles: &mut Handles) -> Result<Call, i64> {
    match nr {
        EXIT => Ok(Call::Exit(args[0] & 0xff)),
        IO => {
            let (handle, op, ptr, len) = (args[0], args[1], args[2], args[3]);
            let need = match op {
                IO_READ => READ,
                IO_WRITE => WRITE,
                _ => return Err(EINVAL),
            };
            let object = handles.get(handle, need)?;
            let len = len.min(MAX_BUFFER);
            user_buffer(ptr, len)?;
            let len = len as usize;
            match object {
                Object::Console if op == IO_WRITE => Ok(Call::Write { ptr, len }),
                Object::Console => Ok(Call::Read { ptr, len }),
                Object::Pipe(end) if end.write == (op == IO_WRITE) => {
                    Ok(Call::Pipe { end, ptr, len })
                }
                Object::Node(inode) => Ok(Call::File {
                    inode,
                    write: op == IO_WRITE,
                    offset: args[4],
                    ptr,
                    len,
                }),
                Object::Dir(_) => Err(EISDIR),
                _ => Err(EACCES),
            }
        }
        DUP => {
            let (handle, object) = handles.dup(args[0], args[1])?;
            Ok(Call::Dup { handle, object })
        }
        CLOSE => handles.close(args[0]).map(Call::Close),
        MAP => match args[0] {
            0 => Err(EINVAL),
            len if len > MAX_MAP => Err(EINVAL),
            len => Ok(Call::Map {
                pages: len.div_ceil(4096) as usize,
            }),
        },
        OPEN => {
            let (ptr, len, flags) = (args[1], args[2], args[3]);
            if flags & !(CREATE | TRUNC) != 0 {
                return Err(EINVAL);
            }
            let (dir, rights) = handles.entry(args[0])?;
            match dir {
                Object::Archive if flags != 0 => return Err(EROFS),
                Object::Archive | Object::Dir(_) => {}
                _ => return Err(ENOTDIR),
            }
            if rights & READ == 0 || (flags != 0 && rights & WRITE == 0) {
                return Err(EACCES);
            }
            user_buffer(ptr, len)?;
            Ok(Call::Open {
                dir,
                ptr,
                len: len as usize,
                flags,
                rights,
            })
        }
        SPAWN => {
            let (exe, ptr, len, budget) = (args[0], args[1], args[2], args[3]);
            let Object::File { start, end } = handles.get(exe, EXEC)? else {
                return Err(EACCES);
            };
            if len > MAX_HANDLES as u64 {
                return Err(EINVAL);
            }
            user_buffer(ptr, len * 8)?;
            Ok(Call::Spawn {
                file: start..end,
                ptr,
                len: len as usize,
                budget: budget as usize,
                priority: args[4],
            })
        }
        PIPE => Ok(Call::NewPipe),
        WAIT => match handles.get(args[0], crate::handle::WAIT)? {
            Object::Process { slot, generation } => Ok(Call::Wait { slot, generation }),
            _ => Err(EACCES),
        },
        MUTEX => Ok(Call::NewMutex),
        LOCK | UNLOCK => match handles.get(args[0], 0)? {
            Object::Mutex(mutex) if nr == LOCK => Ok(Call::Lock(mutex)),
            Object::Mutex(mutex) => Ok(Call::Unlock(mutex)),
            _ => Err(EACCES),
        },
        KILL => match handles.get(args[0], KILL_RIGHT)? {
            Object::Process { slot, generation } => Ok(Call::Kill { slot, generation }),
            _ => Err(EACCES),
        },
        MKDIR | UNLINK => {
            let (dir, ptr, len) = path(handles, args[0], args[1], args[2])?;
            Ok(match nr {
                MKDIR => Call::Mkdir { dir, ptr, len },
                _ => Call::Unlink { dir, ptr, len },
            })
        }
        RENAME => Ok(Call::Rename {
            from: path(handles, args[0], args[1], args[2])?,
            to: path(handles, args[3], args[4], args[5])?,
        }),
        READDIR => {
            let (ptr, len) = (args[1], args[2]);
            let dir = handles.get(args[0], READ)?;
            let (Object::Archive | Object::Dir(_)) = dir else {
                return Err(ENOTDIR);
            };
            user_buffer(ptr, len)?;
            Ok(Call::Readdir {
                dir,
                ptr,
                len: len as usize,
                start: args[3],
            })
        }
        SYNC => match handles.get(args[0], WRITE)? {
            Object::Dir(_) | Object::Node(_) => Ok(Call::Sync),
            _ => Err(ENOTDIR),
        },
        _ => Err(ENOSYS),
    }
}

/// The directory `handle` (write right) and the path buffer `ptr..ptr + len` a call that changes it names.
fn path(handles: &Handles, handle: u64, ptr: u64, len: u64) -> Result<(Inode, u64, usize), i64> {
    let dir = match handles.entry(handle)? {
        (Object::Archive, _) => return Err(EROFS),
        (Object::Dir(_), rights) if rights & WRITE == 0 => return Err(EACCES),
        (Object::Dir(dir), _) => dir,
        _ => return Err(ENOTDIR),
    };
    user_buffer(ptr, len)?;
    Ok((dir, ptr, len as usize))
}

/// `EFAULT` unless `len` is 0 (any `ptr`, as Rust passes empty slices) or `ptr..ptr + len` lies in `USER` and `len`
/// is at most `MAX_BUFFER`.
fn user_buffer(ptr: u64, len: u64) -> Result<(), i64> {
    if len == 0 {
        return Ok(());
    }
    let end = ptr.checked_add(len).ok_or(EFAULT)?;
    if len > MAX_BUFFER || ptr < USER.start || end > USER.end {
        return Err(EFAULT);
    }
    Ok(())
}
