//! Native syscalls (`x8` = number, `x0`-`x4` = arguments, `x0` = result, negative = error) and the panic handler.
#![no_std]

use core::arch::asm;
use core::panic::PanicInfo;

/// Handle 0: the console, for every program spawned with it first.
pub const CONSOLE: u64 = 0;

pub const WRITE: u64 = 1 << 1;
pub const TRANSFER: u64 = 1 << 4;
pub const EXEC: u64 = 1 << 5;

pub const EPERM: i64 = -1;
pub const ENOENT: i64 = -2;
pub const ENOEXEC: i64 = -8;
pub const EBADF: i64 = -9;
pub const ENOMEM: i64 = -12;
pub const EDEADLK: i64 = -35;

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

/// `io_submit_wait(handle, IO_READ, ...)`: 0 at end of file.
pub fn read(handle: u64, buf: &mut [u8]) -> i64 {
    syscall(1, [handle, 0, buf.as_mut_ptr() as u64, buf.len() as u64])
}

/// `io_submit_wait(handle, IO_WRITE, ...)`.
pub fn write(handle: u64, bytes: &[u8]) -> i64 {
    syscall(1, [handle, 1, bytes.as_ptr() as u64, bytes.len() as u64])
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

pub fn open(dir: u64, name: &[u8], rights: u64) -> i64 {
    syscall(5, [dir, name.as_ptr() as u64, name.len() as u64, rights])
}

/// `spawn_at` this process's own priority.
pub fn spawn(exe: u64, handles: &[u64], budget: usize) -> i64 {
    spawn_at(exe, handles, budget, u64::MAX)
}

/// Moves `handles` to the child (at values 0, 1, ...) and `budget` frames of this process's budget; the child runs at
/// `priority` (0 lowest), capped at this process's own.
pub fn spawn_at(exe: u64, handles: &[u64], budget: usize, priority: u64) -> i64 {
    let result;
    // SAFETY: as in `syscall`; `spawn` reads only the handle list.
    unsafe {
        asm!("svc #0", inlateout("x0") exe => result, in("x1") handles.as_ptr(),
            in("x2") handles.len(), in("x3") budget, in("x4") priority, in("x8") 6, options(nostack))
    };
    result
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

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit(1)
}
