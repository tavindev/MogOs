//! Checked access to the current process's memory from a trap.

use core::slice;

use crate::PAGE;

/// Whether `allowed` holds for each page of the `len` bytes at `ptr`.
fn user_pages(ptr: u64, len: usize, allowed: fn(u64) -> bool) -> bool {
    let first_page = ptr & !(PAGE as u64 - 1);
    (first_page..ptr + len as u64).step_by(PAGE).all(allowed)
}

/// The `len` bytes at user address `ptr` (in user space unless `len` is 0, checked by `dispatch`) if EL0 may read all
/// of them; valid only until the trap returns.
pub(crate) fn user_bytes<'a>(ptr: u64, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if !user_pages(ptr, len, arch::user_readable) {
        return None;
    }
    // SAFETY: EL0 may read every page of the range, so it is mapped in the current address space, which
    // stays loaded and unchanged until the trap returns (this core runs it and holds `KERNEL`).
    Some(unsafe { slice::from_raw_parts(ptr as *const u8, len) })
}

/// As `user_bytes`, if EL0 may write all of them.
pub(crate) fn user_bytes_mut<'a>(ptr: u64, len: usize) -> Option<&'a mut [u8]> {
    if len == 0 {
        return Some(&mut []);
    }
    if !user_pages(ptr, len, arch::user_writable) {
        return None;
    }
    // SAFETY: as in `user_bytes`; the pages are user memory, which no kernel reference aliases.
    Some(unsafe { slice::from_raw_parts_mut(ptr as *mut u8, len) })
}
