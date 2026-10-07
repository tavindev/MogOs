# `crates/mogfs` - MogFS, the checksummed copy-on-write file system

## What this crate is

The on-disk format (described at the top of `src/lib.rs`) and `Fs<D: Disk>`: `new` (const), in-place `format` and
`mount`, `disk` (replace only before `mount`), `lookup`, `readdir` (from an entry index, with each kind, stops when
its callback returns true), `kind`, `mkdir`, `create` (opens an existing name), `read`, `write`, `truncate`,
`unlink`, `rename`, `commit`. It also defines the `Disk` trait and `BLOCK_SIZE` that the kernel re-exports and the
board's `VirtioBlk` implements. All in `src/lib.rs`.

It is **NOT** paths, handles or `..` handling (`crates/kernel`, step 22), a block cache beyond its one data buffer, or
a device driver.

## Responsibilities

- Format, mount (newest valid slot; the older one if the newest's table is corrupt), and atomic `commit`.
- Copy-on-write with per-block checksums: a bad block is `Error::Corrupt`, never wrong data.
- Free space derived in memory (`newest`, `committed`, `used`, `replaced` bitmaps and a `free` counter).
- Directories stay packed: `unlink` and a cross-directory `rename` move the last entry into the freed slot, so entry
  order is not creation order. `rename` never replaces a target (`Exists`), does nothing when renaming an entry to
  itself, and rejects moving a directory into itself or below it (`InvalidName`), found by scanning the directories
  below it until the target (none for a move into the root; no parent pointers).

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`.
- Leaf crate: `kernel` and `qemu-virt` depend on it; it depends on none of them.
- `Fs` is about 48 KiB: callers keep it in a static or on the heap (`Fs::new` is `const`), never on a 16 KiB task
  stack; no function in it has a frame above about 1 KiB.

## Invariants & rules

- Spectre v1: `read`, `write` (through `write_data`) and `scan` index a record's block pointers `% PTRS`, in bounds
  by construction, since a position from a user's offset or `readdir` start, on a mispredicted loop bound, runs one
  block past the end; the kernel clamps the offset and start themselves at `dispatch`.

- No block reachable from either superblock slot is written; blocks allocated since the last commit are rewritten in
  place. `commit` writes the dirty table blocks, flushes, writes the other slot, flushes; nothing changed, no I/O.
- Every on-disk value is range-checked once when decoded (superblock, records, directory entries); a crafted image
  gives `Corrupt` (or a fallback mount), never a panic, overflow or out-of-range index. A block reached twice within
  one slot is corrupt.
- Trust note: an image is not checked for two entries naming one inode, so on a crafted image `unlink` can free an
  inode another entry still names.
- `NoSpace` is decided before anything changes (`reserve`). After `Io` from a change, or a failed `mount`, writes and
  commits fail with `Io` until a `mount` succeeds; `Io` from `commit` means the commit may or may not be durable.
- `mount` returns `Io` on any read error; only `Corrupt` falls back to the older slot.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`). The disk requests per operation are asserted exactly
  by `block_io_per_operation`.

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p mogfs` (`tests/fs.rs`: round trip, corruption and fallback,
  unlink and rename, crafted images, power cut (a change with renames, then one with unlinks) through a write-back-cache disk at every write and flush with subsets of the pending
  writes landing, `Io` handling (including `Corrupt` between the removal and the append of a rename), limits, disk request counts).
  `src/tests.rs` runs random changes (including unlink and rename) and commits on a disk that panics on a write to a
  block either slot reaches; it checks the in-memory free space against a fresh mount's, that every in-use inode is
  reached exactly once from the root (in memory every step, and after each commit's remount), and each refused rename
  against an independent subtree walk.
- Benchmark: `cargo bench-host` runs `benches/fs.rs`; baseline rows in `docs/BENCHMARKS.md`.
- `examples/mkfs.rs` writes an empty image through a file-backed `Disk`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
