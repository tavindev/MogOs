# `crates/mogfs` - MogFS, the checksummed copy-on-write file system

## What this crate is

The on-disk format (described at the top of `src/lib.rs`) and `Fs<D: Disk>`: `new` (const), in-place `format` and
`mount`, `lookup`, `readdir`, `kind`, `mkdir`, `create`, `read`, `write`, `truncate`, `commit`. It also defines the
`Disk` trait and `BLOCK_SIZE` that the kernel re-exports and the board's `VirtioBlk` implements. All in `src/lib.rs`.

It is **NOT** paths, handles or `..` handling (`crates/kernel`, step 22), a block cache beyond its one data buffer, or
a device driver.

## Responsibilities

- Format, mount (newest valid slot; the older one if the newest's table is corrupt), and atomic `commit`.
- Copy-on-write with per-block checksums: a bad block is `Error::Corrupt`, never wrong data.
- Free space derived in memory (`newest`, `committed`, `used`, `replaced` bitmaps and a `free` counter).

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`.
- Leaf crate: `kernel` and `qemu-virt` depend on it; it depends on none of them.
- `Fs` is about 48 KiB: callers keep it in a static or on the heap (`Fs::new` is `const`), never on a 16 KiB task
  stack; no function in it has a frame above about 1 KiB.

## Invariants & rules

- No block reachable from either superblock slot is written; blocks allocated since the last commit are rewritten in
  place. `commit` writes the dirty table blocks, flushes, writes the other slot, flushes; nothing changed, no I/O.
- Every on-disk value is range-checked once when decoded (superblock, records, directory entries); a crafted image
  gives `Corrupt` (or a fallback mount), never a panic, overflow or out-of-range index. A block reached twice within
  one slot is corrupt.
- `NoSpace` is decided before anything changes (`reserve`). After `Io` from a change, or a failed `mount`, writes and
  commits fail until a `mount` succeeds; `Io` from `commit` means the commit may or may not be durable.
- `mount` returns `Io` on any read error; only `Corrupt` falls back to the older slot.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`). The block I/O per operation is asserted exactly
  by `block_io_per_operation`.

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p mogfs` (`tests/fs.rs`: round trip, corruption and fallback,
  crafted images, power cut through a write-back-cache disk at every write and flush with subsets of the pending
  writes landing, `Io` handling, limits, block I/O counts). `src/tests.rs` runs random changes and commits on a disk
  that panics on a write to a block either slot reaches, and checks the in-memory free space against a fresh mount's.
- Benchmark: `cargo bench-host` runs `benches/fs.rs`; baseline rows in `docs/BENCHMARKS.md`.
- `examples/mkfs.rs` writes an empty image through a file-backed `Disk`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
