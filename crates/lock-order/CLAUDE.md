# `crates/lock-order` - the lock order, checked at compile time

## What this crate is

Fuchsia netstack3's `lock_order`, cut down: the lock levels as marker types (`Unlocked`, `Process`, `Kernel`, `Net`,
`ProcessTable`, `Frames`, `Console`, and `Leaf` for the heap), `LockAfter<A>` (a level may be taken while one of
level `A` is held, every edge spelled out), the witness `W<'a, L>` (the locks held now are at most level `L`) and
`Held<'a, L>` (the proof a guard keeps). All in `src/lib.rs`, the compile tests in its crate docs.

It is **NOT** a lock: `arch::Lock<T, L>` (`crates/arch/src/aarch64/lock.rs`) takes `&mut W<'_, P>` with
`L: LockAfter<P>` and keeps the `Held` in its guard; `Guard::parts` hands out the data and the child witness, both
borrowing the guard, so no witness outlives its lock.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, workspace `forbid(unsafe_code)`; host-tested (`test-host`
  excludes `arch`, which is why the levels live here).
- Everything is zero-sized: the witnesses cost no instructions (TCG `-icount`: yield, syscall and pipe equal with and
  without them).
- `root()` is safe, so "only at a context holding no lock" is a convention: `arch::root()` is the `unsafe` wrapper the
  board calls at each trap hook's and `Board` method's entry. A second root while locks are held allows an
  out-of-order acquisition (a deadlock, not memory unsafety).

## Invariants & rules

- The order is `Unlocked` < `Process` < `Kernel` < `Net` < `ProcessTable` < `Frames` < `Console`; `Leaf` comes after
  nothing and has nothing after it (`lock_leaf`, no witness). A new level adds an impl for every earlier level, never
  a blanket impl.
- Levels are types, not a const `LEVEL`: stable Rust cannot bound one const generic below another.
- `ProcessTable` takes no lock yet: allocating and freeing a process index (`free_process`, `add_process`, `reap`,
  `close`, `exited`) all run under `KERNEL` (step 26a), so a lock of its own would guard nothing `KERNEL` does not.

## How it's tested

- `cargo test -p lock-order --target aarch64-apple-darwin` runs the eight doctests: four `compile_fail` cases, each
  paired with a compiling one that differs by one line (a process lock under `Kernel`; a second process lock while
  the first guard lives; a witness kept past its guard; `Kernel` under `Console`). Stable rustdoc does not check a
  `compile_fail` block's error code; the codes written there were checked with `rustc` by hand.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
