//! Checked access to the current process's memory from a trap. A sibling thread may write that memory at any time,
//! so the kernel never holds a reference into it: bytes move by raw copy, and inputs the kernel parses are copied in
//! once and then validated. Pointers come from `dispatch`, which clamps them into user space
//! behind its barrier.

use core::ptr;

use crate::PAGE;

/// Whether `allowed` holds for each page of the `len` bytes at `ptr`.
fn user_pages(ptr: u64, len: usize, allowed: fn(u64) -> bool) -> bool {
    let first_page = ptr & !(PAGE as u64 - 1);
    (first_page..ptr + len as u64).step_by(PAGE).all(allowed)
}

/// The `len` bytes at a user address EL0 may read, checked once; `read` copies from them in the same trap, before any
/// switch.
pub(crate) struct UserIn {
    ptr: u64,
    len: usize,
}

impl UserIn {
    /// The `len` bytes at user address `ptr` (in user space unless `len` is 0, checked by `dispatch`) if EL0 may read
    /// all of them.
    pub(crate) fn new(ptr: u64, len: usize) -> Option<Self> {
        (len == 0 || user_pages(ptr, len, arch::user_readable)).then_some(Self { ptr, len })
    }

    /// Copies the bytes at offset `at` into `dst`; panics past the end.
    pub(crate) fn read(&self, at: usize, dst: &mut [u8]) {
        assert!(at + dst.len() <= self.len, "past the user range");
        if dst.is_empty() {
            return;
        }
        // SAFETY: `new` found every page readable by EL0 in the current address space, which stays loaded and mapped
        // until the trap returns (this core holds `KERNEL`); a raw copy, never a reference, as a sibling thread may
        // write it meanwhile.
        unsafe {
            ptr::copy_nonoverlapping(
                (self.ptr + at as u64) as *const u8,
                dst.as_mut_ptr(),
                dst.len(),
            )
        };
    }
}

/// As `UserIn`, for bytes EL0 may write; `write` copies to them.
pub(crate) struct UserOut {
    ptr: u64,
    len: usize,
}

impl UserOut {
    /// The `len` bytes at user address `ptr` (in user space unless `len` is 0, checked by `dispatch`) if EL0 may
    /// write all of them.
    pub(crate) fn new(ptr: u64, len: usize) -> Option<Self> {
        (len == 0 || user_pages(ptr, len, arch::user_writable)).then_some(Self { ptr, len })
    }

    /// Copies `src` to offset `at`; panics past the end.
    pub(crate) fn write(&self, at: usize, src: &[u8]) {
        assert!(at + src.len() <= self.len, "past the user range");
        if src.is_empty() {
            return;
        }
        // SAFETY: as in `UserIn::read`, for pages writable by EL0: user memory, which no kernel reference aliases.
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), (self.ptr + at as u64) as *mut u8, src.len())
        };
    }
}

/// Copies the `len` bytes at user address `ptr` to the start of `buf`, if EL0 may read them; returns the copy, which
/// the kernel then validates and parses.
#[inline]
pub(crate) fn copy_in(ptr: u64, len: usize, buf: &mut [u8]) -> Option<&[u8]> {
    if len == 0 {
        return Some(&[]);
    }
    let input = UserIn::new(ptr, len)?;
    let buf = &mut buf[..len];
    input.read(0, buf);
    Some(buf)
}
