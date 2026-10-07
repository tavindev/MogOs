use core::arch::asm;
use core::cell::{RefCell, UnsafeCell};
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicU16, AtomicU32};

use super::irq;

/// This core's index: TPIDR_EL1's bits 48-63 (bits 0-47 hold its per-CPU area's offset; all 0 on core 0 until
/// `enter_percpu`).
#[inline]
pub fn cpu() -> usize {
    let cpu: usize;
    // SAFETY: reading TPIDR_EL1 has no side effects.
    unsafe {
        asm!("mrs {0}, tpidr_el1", "lsr {0}, {0}, #48", out(reg) cpu, options(nomem, nostack, preserves_flags))
    };
    cpu
}

unsafe extern "C" {
    static __percpu_start: u8;
    static __percpu_end: u8;
}

/// Bytes of the `.percpu` template, so of each core's per-CPU area (a multiple of 16).
pub fn percpu_size() -> usize {
    &raw const __percpu_end as usize - &raw const __percpu_start as usize
}

/// Makes `area` this core's per-CPU area as core `index`: copies the `.percpu` template into it and sets TPIDR_EL1.
///
/// # Safety
///
/// `area` must be 16-byte aligned, below 256 TiB, `percpu_size()` bytes of mapped memory only this core uses from now
/// on; call it once per core, before any `PerCpu` use, with IRQs masked.
pub unsafe fn enter_percpu(index: usize, area: usize) {
    let template = &raw const __percpu_start;
    // SAFETY: the caller guarantees `area` is this core's own, and the template is the linker's `.percpu`.
    unsafe { core::ptr::copy_nonoverlapping(template, area as *mut u8, percpu_size()) };
    let offset = area.wrapping_sub(template as usize) & 0xffff_ffff_ffff;
    // SAFETY: TPIDR_EL1 is read only by `cpu` and `PerCpu`, which now reach the copy just made.
    unsafe {
        asm!("msr tpidr_el1, {}", in(reg) index << 48 | offset, options(nostack, preserves_flags))
    };
}

/// A ticket spinlock: waiters take a ticket and are served in order, each spinning on `ldarh` of the owner ticket.
/// Taking one is an exclusive read-modify-write, so only once the MMU is on.
pub struct Lock<T> {
    next: AtomicU16,
    owner: AtomicU16,
    /// Acquisitions that had to wait (wrapping); in the padding before an 8-byte aligned `data`.
    contended: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands out one reference to `data` at a time, so cores only pass `T` between them.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            next: AtomicU16::new(0),
            owner: AtomicU16::new(0),
            contended: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Masks IRQs, then acquires; the guard releases, then restores the mask.
    pub fn lock(&self) -> Guard<'_, T> {
        let irq = irq::disable();
        let mut guard = self.lock_masked();
        guard.irq = Some(irq);
        guard
    }

    /// Acquires without touching DAIF, for code entered with IRQs masked (trap hooks).
    pub fn lock_masked(&self) -> Guard<'_, T> {
        let ticket = self.next.fetch_add(1, Relaxed);
        if self.owner.load(Acquire) != ticket {
            self.contended.fetch_add(1, Relaxed);
            while self.owner.load(Acquire) != ticket {
                spin_loop();
            }
        }
        Guard {
            lock: self,
            irq: None,
            _data: PhantomData,
        }
    }

    /// Acquisitions so far that had to wait for another holder (wrapping).
    pub fn contended(&self) -> u32 {
        self.contended.load(Relaxed)
    }

    /// Releases the lock a leaked guard held.
    ///
    /// # Safety
    ///
    /// This core holds the lock (through a guard passed to `Guard::leak`), and no reference to its data is used again.
    pub unsafe fn unlock(&self) {
        self.owner
            .store(self.owner.load(Relaxed).wrapping_add(1), Release);
    }
}

/// Exclusive access to a `Lock`'s data until dropped.
#[must_use = "dropping it releases the lock"]
pub struct Guard<'a, T> {
    lock: &'a Lock<T>,
    /// DAIF to restore after the release; `None` from `lock_masked`.
    irq: Option<irq::State>,
    /// Shared only where `T` may be, as the `&T` it derefs to is.
    _data: PhantomData<&'a mut T>,
}

impl<'a, T> Guard<'a, T> {
    /// Keeps the lock held past the guard, until `Lock::unlock`; the IRQ mask stays as it is.
    pub fn leak(guard: Self) -> &'a mut T {
        let guard = core::mem::ManuallyDrop::new(guard);
        // SAFETY: the lock stays held, and without the guard nothing else reaches `data` until `unlock`.
        unsafe { &mut *guard.lock.data.get() }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock, so no other reference to `data` exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the only one from the guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the guard holds the lock and its references ended with the borrow of `self`.
        unsafe { self.lock.unlock() };
        if let Some(irq) = self.irq.take() {
            irq::restore(irq);
        }
    }
}

/// One `T` per core, reached with IRQs masked so the task stays on its core. The static is a template in `.percpu`,
/// never touched: each core reaches its own copy at the static's address plus its TPIDR_EL1 offset.
pub struct PerCpu<T>(RefCell<T>);

// SAFETY: each core touches only its own copy, with IRQs masked, so no copy is shared between cores or reentered by an
// IRQ.
unsafe impl<T: Send> Sync for PerCpu<T> {}

impl<T> PerCpu<T> {
    /// Every core's copy starts as `value`.
    ///
    /// # Safety
    ///
    /// The static must be in the `.percpu` section (`#[unsafe(link_section = ".percpu")]`): `with` reaches it by its
    /// offset from the template.
    pub const unsafe fn new(value: T) -> Self {
        Self(RefCell::new(value))
    }

    /// Runs `f` on this core's `T` with IRQs masked; panics if `f` reaches it again. `f` must not switch tasks, or the
    /// borrow could end on another core.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let irq = irq::disable();
        let offset: isize;
        // SAFETY: reading TPIDR_EL1 has no side effects; `sbfx` sign-extends its offset bits.
        unsafe {
            asm!("mrs {0}, tpidr_el1", "sbfx {0}, {0}, #0, #48", out(reg) offset, options(nomem, nostack, preserves_flags))
        };
        // SAFETY: `new`'s contract puts `self` in the template, and `enter_percpu` made this core's copy at `offset` from
        // it, which only this core reaches, with IRQs masked.
        let copy = unsafe { &*(self as *const Self).wrapping_byte_offset(offset) };
        let mut slot = copy.0.borrow_mut();
        let result = f(&mut slot);
        drop(slot);
        irq::restore(irq);
        result
    }
}
