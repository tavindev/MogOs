//! Native syscalls: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

use crate::handle::{EXEC, Handles, MAX_HANDLES, Object, READ};

/// `exit(code)`: ends the calling process; `wait` reports the low 8 bits of `code`.
const EXIT: u64 = 0;
/// `io_submit_wait(handle, op, ptr, len)`: submits I/O on `handle` and waits for it to complete; returns the bytes
/// moved. `op` is `IO_READ` (into `ptr`, read right) or `IO_WRITE` (from `ptr`, write right). libc's `read` and `write`.
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
/// `spawn(exe, handles_ptr, handles_len, budget)`: starts the executable `exe` (exec right) as a new process, moving
/// it the `handles_len` handles at `handles_ptr` (transfer right; values 0, 1, ... in the child) and `budget` frames of
/// the caller's budget; returns a handle to the process (wait, kill). On failure nothing moves.
const SPAWN: u64 = 6;

/// The exit code `wait` reports for a process a fault killed: outside `exit`'s 0..=255.
pub const KILLED: u64 = 256;

pub const IO_READ: u64 = 0;
pub const IO_WRITE: u64 = 1;

// Errors are negated musl errno values.

/// No such file in the directory.
pub const ENOENT: i64 = -2;
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
/// Bad address: outside user space, unmapped, or longer than `MAX_BUFFER`.
pub const EFAULT: i64 = -14;
/// Invalid argument: a `map` of zero bytes or more than `MAX_MAP`, a `spawn` of more than `MAX_HANDLES` handles, an
/// unknown I/O op.
const EINVAL: i64 = -22;
/// The handle table is full.
pub const EMFILE: i64 = -24;
/// No such syscall.
const ENOSYS: i64 = -38;

/// User virtual addresses: 4 GiB up to the 39-bit VA limit.
const USER: Range<u64> = 1 << 32..1 << 39;
/// Longest user buffer a syscall reads or writes (I/O data, `open` name, `spawn` handles), so its IRQs-masked work
/// stays bounded.
const MAX_BUFFER: u64 = 4096;
/// Longest `map` (16 pages), so its IRQs-masked zeroing stays bounded.
const MAX_MAP: u64 = 16 * 4096;

pub enum Call {
    /// End the caller with this code.
    Exit(u64),
    /// Write to the console; `ptr..ptr + len` lies in `USER` unless empty, but may be unmapped.
    Write { ptr: u64, len: usize },
    /// Map this many pages into the caller's address space.
    Map { pages: usize },
    /// Open the boot archive's file whose name is at `ptr..ptr + len` (in `USER` unless empty, maybe unmapped) with `rights`.
    Open { ptr: u64, len: usize, rights: u64 },
    /// Spawn the boot archive's file `file`, moving the `len` handles at `ptr` (in `USER` unless empty, maybe unmapped) and `budget`.
    Spawn {
        file: Range<usize>,
        ptr: u64,
        len: usize,
        budget: usize,
    },
    /// Done; return this value.
    Done(u64),
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
                IO_WRITE => crate::handle::WRITE,
                _ => return Err(EINVAL),
            };
            let object = handles.get(handle, need)?;
            user_buffer(ptr, len)?;
            let len = len as usize;
            match object {
                Object::Console if op == IO_WRITE => Ok(Call::Write { ptr, len }),
                _ => Err(EACCES),
            }
        }
        DUP => Ok(Call::Done(handles.dup(args[0], args[1])?)),
        CLOSE => handles.close(args[0]).map(|_| Call::Done(0)),
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
            })
        }
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
