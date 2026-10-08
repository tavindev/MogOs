use core::arch::asm;
use core::cell::{RefCell, UnsafeCell};
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicU16, AtomicU32};

use lock_order::{Held, Leaf, LockAfter, Unlocked, W};

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
/// on, and `index` below 65536; call it once per core, before any `PerCpu` use, with IRQs masked.
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

/// The witness of a context holding no lock (`lock_order`), for a trap hook's entry or a board method the kernel
/// crate calls.
///
/// # Safety
///
/// The caller holds no lock, or only locks that every lock it takes under this witness comes after: a witness made
/// while other locks are held would let locks be taken out of order.
pub unsafe fn root() -> W<'static, Unlocked> {
    lock_order::root()
}

/// A ticket spinlock at lock level `L`: waiters take a ticket and are served in order, each spinning on `ldarh` of the
/// owner ticket. Taking one is an exclusive read-modify-write, so only once the MMU is on, and needs the witness of the
/// level held now, which `L` must come after.
pub struct Lock<T, L> {
    next: AtomicU16,
    owner: AtomicU16,
    /// Acquisitions that had to wait (wrapping); in the padding before an 8-byte aligned `data`.
    contended: AtomicU32,
    data: UnsafeCell<T>,
    _level: PhantomData<fn() -> L>,
}

// SAFETY: the lock hands out one reference to `data` at a time, so cores only pass `T` between them.
unsafe impl<T: Send, L> Sync for Lock<T, L> {}

impl<T, L> Lock<T, L> {
    pub const fn new(data: T) -> Self {
        Self {
            next: AtomicU16::new(0),
            owner: AtomicU16::new(0),
            contended: AtomicU32::new(0),
            data: UnsafeCell::new(data),
            _level: PhantomData,
        }
    }

    /// The data, without taking the lock.
    ///
    /// # Safety
    ///
    /// Nothing else reaches the data while the reference lives: it is not yet published to another core or task.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn unpublished(&self) -> &mut T {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.data.get() }
    }

    /// Masks IRQs, then acquires under the witness `w`; the guard releases, then restores the mask.
    pub fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Guard<'a, T, L>
    where
        L: LockAfter<P>,
    {
        let irq = irq::disable();
        let mut guard = self.lock_masked(w);
        guard.irq = Some(irq);
        guard
    }

    /// Acquires under the witness `w` without touching DAIF, for code entered with IRQs masked (trap hooks).
    pub fn lock_masked<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Guard<'a, T, L>
    where
        L: LockAfter<P>,
    {
        self.acquire();
        Guard {
            lock: self,
            irq: None,
            held: w.after(),
            _data: PhantomData,
        }
    }

    fn acquire(&self) {
        let ticket = self.next.fetch_add(1, Relaxed);
        if self.owner.load(Acquire) != ticket {
            self.contended.fetch_add(1, Relaxed);
            while self.owner.load(Acquire) != ticket {
                spin_loop();
            }
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

impl<T> Lock<T, Leaf> {
    /// Masks IRQs, then acquires a leaf lock, which needs no witness: nothing is taken under it.
    pub fn lock_leaf(&self) -> Guard<'_, T, Leaf> {
        let irq = irq::disable();
        self.acquire();
        Guard {
            lock: self,
            irq: Some(irq),
            held: lock_order::leaf(),
            _data: PhantomData,
        }
    }
}

// `Guard` is `Sync` only for a `Sync` `T`: shared, it hands out the `&T` it derefs to (this fails to build otherwise).
const _: () = {
    trait AmbiguousIfSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
    let _ = <Guard<'static, core::cell::Cell<u8>, Leaf> as AmbiguousIfSync<_>>::check;
};

/// Exclusive access to a `Lock`'s data until dropped; locks taken under it use its witness (`parts`).
#[must_use = "dropping it releases the lock"]
pub struct Guard<'a, T, L> {
    lock: &'a Lock<T, L>,
    /// DAIF to restore after the release; `None` from `lock_masked`.
    irq: Option<irq::State>,
    /// The order proof, borrowing the witness the lock was taken under.
    held: Held<'a, L>,
    /// Shared only where `T` may be, as the `&T` it derefs to is.
    _data: PhantomData<&'a mut T>,
}

impl<'a, T, L> Guard<'a, T, L> {
    /// Keeps the lock held past the guard, until `Lock::unlock`; the IRQ mask stays as it is. Returns the data and the
    /// witness for locks taken under it.
    pub fn leak(guard: Self) -> (&'a mut T, W<'a, L>) {
        let guard = core::mem::ManuallyDrop::new(guard);
        // SAFETY: the guard is never dropped, so its proof is not read again after this copy.
        let held = unsafe { core::ptr::read(&guard.held) };
        // SAFETY: the lock stays held, and without the guard nothing else reaches `data` until `unlock`.
        (unsafe { &mut *guard.lock.data.get() }, held.into_witness())
    }

    /// The data and the witness for locks taken under this one, both borrowing the guard.
    pub fn parts(&mut self) -> (&mut T, W<'_, L>) {
        // SAFETY: the guard holds the lock, and `&mut self` makes this the only reference from it.
        (unsafe { &mut *self.lock.data.get() }, self.held.witness())
    }
}

impl<T, L> Deref for Guard<'_, T, L> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock, so no other reference to `data` exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T, L> DerefMut for Guard<'_, T, L> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the only one from the guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T, L> Drop for Guard<'_, T, L> {
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
        // SAFETY: IRQs are masked above.
        let result = unsafe { self.with_masked(f) };
        irq::restore(irq);
        result
    }

    /// As `with`, without touching DAIF.
    ///
    /// # Safety
    ///
    /// IRQs must be masked (trap context), so the task stays on its core while `f` runs.
    #[inline(always)]
    pub unsafe fn with_masked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let offset: isize;
        // SAFETY: reading TPIDR_EL1 has no side effects; `sbfx` sign-extends its offset bits.
        unsafe {
            asm!("mrs {0}, tpidr_el1", "sbfx {0}, {0}, #0, #48", out(reg) offset, options(nomem, nostack, preserves_flags))
        };
        // SAFETY: `new`'s contract puts `self` in the template, and `enter_percpu` made this core's copy at `offset` from
        // it, which only this core reaches, with IRQs masked (the caller's contract).
        let copy = unsafe {
            &*core::ptr::with_exposed_provenance::<Self>(
                (self as *const Self)
                    .expose_provenance()
                    .wrapping_add_signed(offset),
            )
        };
        f(&mut copy.0.borrow_mut())
    }
}
