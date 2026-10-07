//! Native syscalls: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

use crate::handle::{EXEC, Handles, KILL as KILL_RIGHT, MAX_HANDLES, Object, READ, WRITE};
use crate::mutex::Mutex;
use crate::pipe::End;

/// `exit(code)`: ends the calling process; `wait` reports the low 8 bits of `code`.
const EXIT: u64 = 0;
/// `io_submit_wait(handle, op, ptr, len)`: submits I/O on `handle` and waits for it to complete; returns the bytes
/// moved. `op` is `IO_READ` (into `ptr`, read right) or `IO_WRITE` (from `ptr`, write right). libc's `read` and `write`.
/// A `len` over `MAX_BUFFER` moves at most `MAX_BUFFER` bytes (a short read or write). Reading the console waits for a
/// line (`console::Line`); with two readers, whichever runs first gets it.
const IO: u64 = 1;
/// `dup(handle, rights)`: returns a new handle to the same object with `rights`, a subset of `handle`'s (duplicate right).
const DUP: u64 = 2;
/// `close(handle)`: returns 0.
const CLOSE: u64 = 3;
/// `map(len)`: maps `len` bytes (rounded up to pages) of zeroed read-write memory at an address the kernel picks,
/// charged to the caller's budget; returns the address. The kernel picking the address leaves nothing to overlap, and
/// it is what musl's `mmap(NULL, ...)` needs (its malloc falls back from `brk` to `mmap`).
const MAP: u64 = 4;
/// `open(dir, name_ptr, name_len, rights)`: returns a handle with `rights` to the file `name` in directory `dir`, which
/// needs read and every right in `rights`. Names resolve only relative to a directory handle.
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

/// The exit code `wait` reports for a process a fault killed: outside `exit`'s 0..=255.
pub const KILLED: u64 = 256;

pub const IO_READ: u64 = 0;
pub const IO_WRITE: u64 = 1;

// Errors are negated musl errno values.

/// Unlocking a mutex the caller does not own.
pub const EPERM: i64 = -1;
/// No such file in the directory.
pub const ENOENT: i64 = -2;
/// The disk failed a request.
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
/// Invalid argument: a `map` of zero bytes or more than `MAX_MAP`, a `spawn` of more than `MAX_HANDLES` handles, an
/// unknown I/O op.
const EINVAL: i64 = -22;
/// The pipe or mutex table is full.
pub const ENFILE: i64 = -23;
/// The handle table is full.
pub const EMFILE: i64 = -24;
/// Writing a pipe with no read end left.
pub const EPIPE: i64 = -32;
/// Locking a mutex the caller owns.
pub const EDEADLK: i64 = -35;
/// No such syscall.
const ENOSYS: i64 = -38;

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
    /// Open the boot archive's file whose name is at `ptr..ptr + len` (in `USER` unless empty, maybe unmapped) with `rights`.
    Open {
        ptr: u64,
        len: usize,
        rights: u64,
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
            let (dir, ptr, len, rights) = (args[0], args[1], args[2], args[3]);
            let Object::Archive = handles.get(dir, READ | rights)? else {
                return Err(EACCES);
            };
            user_buffer(ptr, len)?;
            Ok(Call::Open {
                ptr,
                len: len as usize,
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
        _ => Err(ENOSYS),
    }
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
