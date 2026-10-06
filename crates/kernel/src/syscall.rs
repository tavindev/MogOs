//! Native syscalls: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

use crate::handle::{Handles, Object};

/// `exit(code)`: ends the calling process.
const EXIT: u64 = 0;
/// `write(handle, ptr, len)`: writes `len` bytes at `ptr` through `handle` (write right); returns `len`.
const WRITE: u64 = 1;
/// `dup(handle, rights)`: returns a new handle to the same object with `rights`, a subset of `handle`'s (duplicate right).
const DUP: u64 = 2;
/// `close(handle)`: returns 0.
const CLOSE: u64 = 3;
/// `map(len)`: maps `len` bytes (rounded up to pages) of zeroed read-write memory at an address the kernel picks,
/// charged to the caller's budget; returns the address. The kernel picking the address leaves nothing to overlap, and
/// it is what musl's `mmap(NULL, ...)` needs (its malloc falls back from `brk` to `mmap`).
const MAP: u64 = 4;

// Errors are negated musl errno values.

/// Bad, closed or stale handle.
pub const EBADF: i64 = -9;
/// Over the memory budget, or out of frames.
pub const ENOMEM: i64 = -12;
/// The handle lacks a right the call needs.
pub const EACCES: i64 = -13;
/// Bad address: outside user space, unmapped, or longer than `MAX_WRITE`.
pub const EFAULT: i64 = -14;
/// Invalid argument: a zero-length `map`.
const EINVAL: i64 = -22;
/// The handle table is full.
pub const EMFILE: i64 = -24;
/// No such syscall.
const ENOSYS: i64 = -38;

/// User virtual addresses: 4 GiB up to the 39-bit VA limit.
const USER: Range<u64> = 1 << 32..1 << 39;
/// Longest `write`, so the syscall's IRQs-masked work stays bounded.
const MAX_WRITE: u64 = 4096;

pub enum Call {
    Exit,
    /// Write to the console; `ptr..ptr + len` lies in `USER` but may be unmapped.
    Write {
        ptr: u64,
        len: usize,
    },
    /// Map this many pages into the caller's address space.
    Map {
        pages: usize,
    },
    /// Done; return this value.
    Done(u64),
}

/// Runs syscall `nr` with arguments `args` (`x0`-`x5`) against the caller's `handles`, leaving the board the parts
/// that touch hardware or tasks; `Err` holds the result to return.
pub fn dispatch(nr: u64, args: &[u64; 6], handles: &mut Handles) -> Result<Call, i64> {
    match nr {
        EXIT => Ok(Call::Exit),
        WRITE => {
            let (handle, ptr, len) = (args[0], args[1], args[2]);
            let Object::Console = handles.get(handle, crate::handle::WRITE)? else {
                return Err(EACCES);
            };
            let end = ptr.checked_add(len).ok_or(EFAULT)?;
            if len > MAX_WRITE || ptr < USER.start || end > USER.end {
                return Err(EFAULT);
            }
            Ok(Call::Write {
                ptr,
                len: len as usize,
            })
        }
        DUP => Ok(Call::Done(handles.dup(args[0], args[1])?)),
        CLOSE => handles.close(args[0]).map(|()| Call::Done(0)),
        MAP => match args[0] {
            0 => Err(EINVAL),
            len => Ok(Call::Map {
                pages: len.div_ceil(4096) as usize,
            }),
        },
        _ => Err(ENOSYS),
    }
}
