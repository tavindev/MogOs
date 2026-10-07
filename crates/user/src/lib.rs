//! Native syscalls (`x8` = number, `x0`-`x5` = arguments, `x0` = result, negative = error) and the panic handler.
#![no_std]

use core::arch::asm;
use core::panic::PanicInfo;

/// Handle 0: the console, for every program spawned with it first.
pub const CONSOLE: u64 = 0;

pub const WRITE: u64 = 1 << 1;
pub const TRANSFER: u64 = 1 << 4;

/// `open` flags: create a missing file; empty the file.
pub const CREATE: u64 = 1 << 0;
pub const TRUNC: u64 = 1 << 1;

/// Where the stack ends: the board's `USER_STACK_TOP`.
const STACK_TOP: usize = (1 << 32) + (2 << 20);
/// Most arguments `spawn` passes.
pub const MAX_ARGS: usize = 32;

pub const EPERM: i64 = -1;
pub const ENOENT: i64 = -2;
pub const EIO: i64 = -5;
pub const E2BIG: i64 = -7;
pub const ENOEXEC: i64 = -8;
pub const EBADF: i64 = -9;
pub const ENOMEM: i64 = -12;
pub const EACCES: i64 = -13;
pub const EFAULT: i64 = -14;
pub const EBUSY: i64 = -16;
pub const EEXIST: i64 = -17;
pub const ENOTDIR: i64 = -20;
pub const EISDIR: i64 = -21;
pub const EINVAL: i64 = -22;
pub const EFBIG: i64 = -27;
pub const ENOSPC: i64 = -28;
pub const EROFS: i64 = -30;
pub const EDEADLK: i64 = -35;
pub const ENOTEMPTY: i64 = -39;

/// The exit code `wait` reports for a killed process.
pub const KILLED: i64 = 256;

fn syscall(nr: u64, args: [u64; 4]) -> i64 {
    let result;
    // SAFETY: `svc` enters the kernel, which reads only the memory the arguments describe and clobbers only x0.
    unsafe {
        asm!("svc #0", inlateout("x0") args[0] => result, in("x1") args[1], in("x2") args[2],
            in("x3") args[3], in("x8") nr, options(nostack))
    };
    result
}

pub fn exit(code: u64) -> ! {
    syscall(0, [code, 0, 0, 0]);
    loop {
        core::hint::spin_loop()
    }
}

/// `io_submit_wait(handle, op, ptr, len, offset)`; files need `offset`, the console and pipes ignore it.
fn io(handle: u64, op: u64, (ptr, len): (u64, usize), offset: u64) -> i64 {
    let result;
    // SAFETY: as in `syscall`; the kernel reads or writes only the `len` bytes at `ptr`.
    unsafe {
        asm!("svc #0", inlateout("x0") handle => result, in("x1") op, in("x2") ptr, in("x3") len,
            in("x4") offset, in("x8") 1, options(nostack))
    };
    result
}

/// `read_at` offset 0.
pub fn read(handle: u64, buf: &mut [u8]) -> i64 {
    read_at(handle, buf, 0)
}

/// Reads into `buf` from `offset`; 0 at end of file.
pub fn read_at(handle: u64, buf: &mut [u8], offset: u64) -> i64 {
    io(handle, 0, (buf.as_mut_ptr() as u64, buf.len()), offset)
}

/// `write_at` offset 0.
pub fn write(handle: u64, bytes: &[u8]) -> i64 {
    write_at(handle, bytes, 0)
}

pub fn write_at(handle: u64, bytes: &[u8], offset: u64) -> i64 {
    io(handle, 1, (bytes.as_ptr() as u64, bytes.len()), offset)
}

/// Writes `n` in decimal.
pub fn write_u64(handle: u64, mut n: u64) {
    let mut digits = [0; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    write(handle, &digits[i..]);
}

pub fn dup(handle: u64, rights: u64) -> i64 {
    syscall(2, [handle, rights, 0, 0])
}

pub fn close(handle: u64) -> i64 {
    syscall(3, [handle, 0, 0, 0])
}

/// `len` bytes of fresh zeroed memory, or `None` (`ENOMEM`, `EINVAL`).
pub fn map(len: usize) -> Option<&'static mut [u8]> {
    let addr = syscall(4, [len as u64, 0, 0, 0]);
    if addr < 0 {
        return None;
    }
    // SAFETY: the kernel mapped `len` zeroed read-write bytes at `addr` for this process alone and never unmaps them.
    Some(unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) })
}

/// Opens the file or directory at `path` (`/`-separated, relative to `dir`) with `dir`'s rights; `flags`: `CREATE`,
/// `TRUNC`.
pub fn open(dir: u64, path: &[u8], flags: u64) -> i64 {
    syscall(5, [dir, path.as_ptr() as u64, path.len() as u64, flags])
}

/// `spawn_at` this process's own priority, with no arguments.
pub fn spawn(exe: u64, handles: &[u64], budget: usize) -> i64 {
    spawn_at(exe, handles, budget, u64::MAX, &[])
}

/// Moves `handles` to the child (at values 0, 1, ...) and `budget` frames of this process's budget; the child runs at
/// `priority` (0 lowest), capped at this process's own, with `args`: strings each ending in a NUL, at most `MAX_ARGS`
/// and 4096 bytes (`E2BIG`).
pub fn spawn_at(exe: u64, handles: &[u64], budget: usize, priority: u64, args: &[u8]) -> i64 {
    let result;
    // SAFETY: as in `syscall`; `spawn` reads only the handle list and the arguments.
    unsafe {
        asm!("svc #0", inlateout("x0") exe => result, in("x1") handles.as_ptr(),
            in("x2") handles.len(), in("x3") budget, in("x4") priority, in("x5") args.as_ptr(),
            in("x6") args.len(), in("x8") 6, options(nostack))
    };
    result
}

/// Runs `main` with the arguments `spawn` passed and exits with its result: `_start` receives x0 = their count, x1 =
/// their address, x2 = their length (0, none, for a process spawned at boot), and passes the count and length here.
pub fn start(argc: usize, len: usize, main: fn(&[&[u8]]) -> u64) -> ! {
    let len = len.min(4096);
    // SAFETY: the kernel copies the arguments to the end of the top stack page, which stays mapped; `len` fits in it.
    let bytes = unsafe { core::slice::from_raw_parts((STACK_TOP - len) as *const u8, len) };
    let mut args: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
    for (arg, bytes) in args.iter_mut().zip(bytes.split(|&b| b == 0)) {
        *arg = bytes;
    }
    exit(main(&args[..argc.min(MAX_ARGS)]))
}

/// Returns the read end (or an error) and the write end.
pub fn pipe() -> (i64, u64) {
    let (read, write);
    // SAFETY: as in `syscall`; `pipe` writes only x0 and x1.
    unsafe {
        asm!("svc #0", lateout("x0") read, lateout("x1") write, in("x8") 7, options(nostack))
    };
    (read, write)
}

/// Blocks until `process` exits; returns its exit code.
pub fn wait(process: u64) -> i64 {
    syscall(8, [process, 0, 0, 0])
}

pub fn mutex() -> i64 {
    syscall(9, [0, 0, 0, 0])
}

/// Blocks until `mutex` is free, then owns it.
pub fn lock(mutex: u64) -> i64 {
    syscall(10, [mutex, 0, 0, 0])
}

pub fn unlock(mutex: u64) -> i64 {
    syscall(11, [mutex, 0, 0, 0])
}

pub fn kill(process: u64) -> i64 {
    syscall(12, [process, 0, 0, 0])
}

pub fn mkdir(dir: u64, path: &[u8]) -> i64 {
    syscall(13, [dir, path.as_ptr() as u64, path.len() as u64, 0])
}

/// Fills `buf` with whole `name\n` entries (`name/\n` for a directory) from entry `start` on; returns the bytes
/// written, 0 at the end.
pub fn readdir(dir: u64, buf: &mut [u8], start: u64) -> i64 {
    syscall(14, [dir, buf.as_mut_ptr() as u64, buf.len() as u64, start])
}

/// Makes every change to the file system durable; `EIO` leaves it unknown whether it did.
pub fn sync(dir: u64) -> i64 {
    syscall(15, [dir, 0, 0, 0])
}

/// Removes the file or empty directory at `path` under `dir`.
pub fn unlink(dir: u64, path: &[u8]) -> i64 {
    syscall(16, [dir, path.as_ptr() as u64, path.len() as u64, 0])
}

/// Moves the entry at `from` under `from_dir` to `to` under `to_dir`; `EEXIST` if `to` exists.
pub fn rename(from_dir: u64, from: &[u8], to_dir: u64, to: &[u8]) -> i64 {
    let result;
    // SAFETY: as in `syscall`; `rename` reads only the two paths.
    unsafe {
        asm!("svc #0", inlateout("x0") from_dir => result, in("x1") from.as_ptr(), in("x2") from.len(),
            in("x3") to_dir, in("x4") to.as_ptr(), in("x5") to.len(), in("x8") 17, options(nostack))
    };
    result
}

/// Nanoseconds on the virtual counter.
pub fn now_ns() -> u64 {
    let (count, freq): (u64, u64);
    // SAFETY: the kernel lets EL0 read the virtual counter and its frequency, which has no side effects.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", "mrs {}, cntfrq_el0", out(reg) count, out(reg) freq,
            options(nomem, nostack))
    };
    (count as u128 * 1_000_000_000 / freq as u128) as u64
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit(1)
}
