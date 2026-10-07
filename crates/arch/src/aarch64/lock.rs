use core::arch::asm;
use core::cell::{RefCell, UnsafeCell};
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::AtomicU16;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use super::irq;

/// Cores `PerCpu` has a slot for.
pub const MAX_CPUS: usize = 4;

/// This core's index, from TPIDR_EL1 (set at boot).
pub fn cpu() -> usize {
    let cpu: usize;
    // SAFETY: reading TPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) cpu, options(nomem, nostack, preserves_flags)) };
    cpu
}

/// A ticket spinlock: waiters take a ticket and are served in order, each spinning on `ldarh` of the owner ticket.
/// Taking one is an exclusive read-modify-write, so only once the MMU is on.
pub struct Lock<T> {
    next: AtomicU16,
    owner: AtomicU16,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands out one reference to `data` at a time, so cores only pass `T` between them.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            next: AtomicU16::new(0),
            owner: AtomicU16::new(0),
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
        while self.owner.load(Acquire) != ticket {
            spin_loop();
        }
        Guard {
            lock: self,
            irq: None,
            _data: PhantomData,
        }
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

/// One `T` per core, reached with IRQs masked so the task stays on its core.
pub struct PerCpu<T>([RefCell<T>; MAX_CPUS]);

// SAFETY: core `i` touches only slot `i`, with IRQs masked, so no slot is shared between cores or reentered by an IRQ.
unsafe impl<T: Send> Sync for PerCpu<T> {}

impl<T> PerCpu<T> {
    /// Every core's slot starts as `value`.
    pub const fn new(value: T) -> Self
    where
        T: Copy,
    {
        Self([
            RefCell::new(value),
            RefCell::new(value),
            RefCell::new(value),
            RefCell::new(value),
        ])
    }

    /// Runs `f` on this core's `T` with IRQs masked; panics if `f` reaches it again. `f` must not switch tasks: once
    /// tasks migrate (step 25b) the borrow could end on another core.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let irq = irq::disable();
        let mut slot = self.0[cpu()].borrow_mut();
        let result = f(&mut slot);
        drop(slot);
        irq::restore(irq);
        result
    }
}
