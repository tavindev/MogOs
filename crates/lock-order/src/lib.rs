//! Lock order checked at compile time (Fuchsia netstack3's `lock_order`). Each lock has a level, a marker type; a
//! level implements `LockAfter<A>` for every level `A` that may be held while it is taken, each edge spelled out. Taking
//! a lock needs `&mut` the witness `W` of the level held now, and the guard keeps a `Held` whose own witness (for
//! locks taken under it) borrows the guard: nothing taken under a lock outlives it, and a lock out of order does not
//! compile. Witnesses are zero-sized.
//!
//! The order: `Unlocked`, then `Process`, `Kernel`, `Net`, `ProcessTable`, `Frames`, `Console`. `Leaf` (the heap)
//! comes after nothing and has nothing after it: it is taken without a witness.
//!
//! # Compile tests
//!
//! Each failing case is paired with one that compiles and differs by one line. The lock below stands in for
//! `arch::Lock`, which takes a witness the same way. Stable rustdoc does not check a `compile_fail` block's error code,
//! so each code written here was checked with `rustc` by hand (a process lock has one earlier level, so its order error
//! reads as a type mismatch).
//!
//! A process lock under `Kernel` does not compile; `Kernel` under a process lock does:
//!
//! ```compile_fail,E0308
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (process, kernel) = (Lock::<Process>(Default::default()), Lock::<Kernel>(Default::default()));
//! let mut root = root();
//! let mut k = kernel.lock(&mut root);
//! let _p = process.lock(&mut k.witness());
//! ```
//!
//! ```
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (process, kernel) = (Lock::<Process>(Default::default()), Lock::<Kernel>(Default::default()));
//! let mut root = root();
//! let mut p = process.lock(&mut root);
//! let _k = kernel.lock(&mut p.witness());
//! ```
//!
//! A second process lock while the first guard lives does not compile; after the first guard's scope it does:
//!
//! ```compile_fail,E0499
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (a, b) = (Lock::<Process>(Default::default()), Lock::<Process>(Default::default()));
//! let mut root = root();
//! let first = a.lock(&mut root);
//! let _second = b.lock(&mut root);
//! drop(first);
//! ```
//!
//! ```
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (a, b) = (Lock::<Process>(Default::default()), Lock::<Process>(Default::default()));
//! let mut root = root();
//! let first = a.lock(&mut root);
//! drop(first);
//! let _second = b.lock(&mut root);
//! ```
//!
//! A witness kept past its guard and used under a later lock does not compile; used inside the guard's scope it does:
//!
//! ```compile_fail,E0505
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (kernel, console) = (Lock::<Kernel>(Default::default()), Lock::<Console>(Default::default()));
//! let mut root = root();
//! let mut k = kernel.lock(&mut root);
//! let mut w = k.witness();
//! drop(k);
//! let _c = console.lock(&mut w);
//! ```
//!
//! ```
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (kernel, console) = (Lock::<Kernel>(Default::default()), Lock::<Console>(Default::default()));
//! let mut root = root();
//! let mut k = kernel.lock(&mut root);
//! let mut w = k.witness();
//! let _c = console.lock(&mut w);
//! drop(k);
//! ```
//!
//! `Kernel` under `Console` does not compile; `Console` under `Kernel` does:
//!
//! ```compile_fail,E0277
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (kernel, console) = (Lock::<Kernel>(Default::default()), Lock::<Console>(Default::default()));
//! let mut root = root();
//! let mut c = console.lock(&mut root);
//! let _k = kernel.lock(&mut c.witness());
//! ```
//!
//! ```
//! # use lock_order::*;
//! # struct Lock<L>(core::marker::PhantomData<L>);
//! # impl<L> Lock<L> {
//! #     fn lock<'a, P>(&'a self, w: &'a mut W<'_, P>) -> Held<'a, L> where L: LockAfter<P> { w.after() }
//! # }
//! # let (kernel, console) = (Lock::<Kernel>(Default::default()), Lock::<Console>(Default::default()));
//! let mut root = root();
//! let mut k = kernel.lock(&mut root);
//! let _c = console.lock(&mut k.witness());
//! ```
#![cfg_attr(not(test), no_std)]

use core::marker::PhantomData;

/// A lock of this level may be taken while one of level `A` is held.
pub trait LockAfter<A> {}

/// The witness that the locks held now are at most level `L`; taking a lock borrows it mutably.
pub struct W<'a, L>(PhantomData<&'a mut fn() -> L>);

/// Proof that a lock of level `L` is held, kept by its guard; it borrows the witness it was taken under.
pub struct Held<'a, L>(PhantomData<&'a mut fn() -> L>);

impl<L> W<'_, L> {
    /// Takes a lock of level `M` under this witness: the proof borrows the witness for its whole life.
    pub fn after<M: LockAfter<L>>(&mut self) -> Held<'_, M> {
        Held(PhantomData)
    }
}

impl<L> Held<'_, L> {
    /// The witness for locks taken under this one; it borrows the proof, so it cannot outlive the guard.
    pub fn witness(&mut self) -> W<'_, L> {
        W(PhantomData)
    }

    /// The witness for a lock kept held past its guard (a trap hook's leaked lock), for the rest of the proof's life.
    pub fn into_witness<'a>(self) -> W<'a, L>
    where
        Self: 'a,
    {
        W(PhantomData)
    }
}

/// The witness of a context holding no lock. By convention only `arch` calls it, behind its `unsafe` root constructor
/// (this crate is safe, so nothing enforces that): a second root while locks are held would let them be taken out of
/// order (a deadlock, not memory unsafety).
pub fn root() -> W<'static, Unlocked> {
    W(PhantomData)
}

/// The proof for a `Leaf` lock, which is taken without a witness.
pub fn leaf() -> Held<'static, Leaf> {
    Held(PhantomData)
}

/// Holding no lock.
pub struct Unlocked;
/// A process's lock: its handle table writes, map cursor and (step 27) futex waiters.
pub struct Process;
/// `KERNEL`: the scheduler, pipes, mutexes, the console line and the file system.
pub struct Kernel;
/// The network and its setup.
pub struct Net;
/// The process table's free masks and generations.
pub struct ProcessTable;
/// The frame allocator.
pub struct Frames;
/// Console output; nothing is taken under it.
pub struct Console;
/// The heap: taken without a witness (from `GlobalAlloc`), and nothing after it.
pub struct Leaf;

impl LockAfter<Unlocked> for Process {}
impl LockAfter<Unlocked> for Kernel {}
impl LockAfter<Process> for Kernel {}
impl LockAfter<Unlocked> for Net {}
impl LockAfter<Process> for Net {}
impl LockAfter<Kernel> for Net {}
impl LockAfter<Unlocked> for ProcessTable {}
impl LockAfter<Process> for ProcessTable {}
impl LockAfter<Kernel> for ProcessTable {}
impl LockAfter<Net> for ProcessTable {}
impl LockAfter<Unlocked> for Frames {}
impl LockAfter<Process> for Frames {}
impl LockAfter<Kernel> for Frames {}
impl LockAfter<Net> for Frames {}
impl LockAfter<ProcessTable> for Frames {}
impl LockAfter<Unlocked> for Console {}
impl LockAfter<Process> for Console {}
impl LockAfter<Kernel> for Console {}
impl LockAfter<Net> for Console {}
impl LockAfter<ProcessTable> for Console {}
impl LockAfter<Frames> for Console {}
