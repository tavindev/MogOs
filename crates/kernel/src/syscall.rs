//! Native syscalls: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

/// `exit(code)`: ends the calling process.
pub const EXIT: u64 = 0;
/// `print(ptr, len)`: writes `len` bytes at `ptr` to the console; returns 0.
pub const PRINT: u64 = 1;

/// Bad address: outside user space, unmapped, or longer than `MAX_PRINT`.
pub const EFAULT: i64 = -14;
/// No such syscall.
pub const ENOSYS: i64 = -38;

/// User virtual addresses: 4 GiB up to the 39-bit VA limit.
pub const USER: Range<u64> = 1 << 32..1 << 39;
/// Longest `print`, so the syscall's IRQs-masked work stays bounded.
pub const MAX_PRINT: u64 = 4096;

pub enum Call {
    Exit,
    /// `ptr..ptr + len` lies in `USER` but may be unmapped.
    Print {
        ptr: u64,
        len: usize,
    },
}

/// Decodes syscall `nr` with arguments `args` (`x0`-`x5`); `Err` holds the result to return.
pub fn decode(nr: u64, args: &[u64]) -> Result<Call, i64> {
    match nr {
        EXIT => Ok(Call::Exit),
        PRINT => {
            let (ptr, len) = (args[0], args[1]);
            let end = ptr.checked_add(len).ok_or(EFAULT)?;
            if len > MAX_PRINT || ptr < USER.start || end > USER.end {
                return Err(EFAULT);
            }
            Ok(Call::Print {
                ptr,
                len: len as usize,
            })
        }
        _ => Err(ENOSYS),
    }
}
