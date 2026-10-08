# `crates/mogfs2` - MogFS v2, the copy-on-write B+tree file system

## What this crate is

The v2 on-disk format (described at the top of `src/lib.rs`) and `Fs<'a, D: Disk>` over memory its caller gives
(`cache_blocks`, `bitmap_words`): `new` (const), in-place `format(seed)` and `mount`, `disk`, `set_time`, `height`,
`lookup`, `readdir` (from an opaque u64 cursor; returns the cursor of the entry its callback stopped at, `u64::MAX` past
the end), `kind`, `stat`, `mkdir`, `create` (opens an existing name), `read`, `write`, `truncate`, `unlink`,
`rename`, `map(file, Page) -> (Block, Sum)` with the free function `verify`, and `commit`. Its `Disk` trait is a copy
of v1's (block numbers stay `u64` there; `Block` converts at that boundary), over `Buf` (one block's bytes). Inode,
block, page, sum and key offset are `#[repr(transparent)]` newtypes; a tree key is built from its
parts only by `Key::new(Inode, ItemKind, Offset)`.
Phase 7 step 39; it replaces `crates/mogfs` (and takes back its name) in step 39b. All in `src/lib.rs`.

It is **NOT** paths, handles or `..` handling (`crates/kernel`), a page cache, snapshots (step 40) or scrub (step 41),
or a device driver.

## Responsibilities

- Format, mount (newest valid slot; the older one if the newest's bitmap or rightmost path is corrupt), atomic
  `commit`.
- One copy-on-write B+tree of inode, directory entry and extent items; every pointer holds its child's sum and birth
  generation; data pages are whole 4096-byte blocks whose sums live in their extent (at most 128 pages).
- Free space stored per root as a bitmap (pages of 128 MiB under one index, so at most about 32 GiB; a larger format
  takes an incompatible feature flag), with the other slot's bitmap kept reserved.
- Directory entries keyed by a seeded name hash with a collision chain of 8; a full chain is `Collision`. The bound,
  not the hash's strength, is the guarantee (the hash is a seeded multiply-rotate; tests build collisions from the
  seed).
- Inode numbers come from a counter and are never reused; mount checks the counter against the rightmost path.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`.
- Leaf crate; nothing in the workspace depends on it until step 39b.
- `Fs` is about 22 KiB plus the caller's memory: `cache` (one index slot, the bitmap pages, 32 commit staging slots and
  a node pool of 32 to 512 slots) and `bits` (three bitmaps). A disk past that memory is `TooBig` at mount.

## Invariants & rules

- No block reachable from either slot is written. Dirty nodes stay in their cache slots; `commit` gives them, the
  changed bitmap pages and a new index one free run, copies the nodes after the pages in the staging slots and writes
  them in one request, flushes, writes the other slot, flushes (`[0, 2, 2]` while a run fits and the staging slots
  hold them). When the pool runs short, dirty nodes are written out early (not a commit) at the start of a tree
  operation, never in the middle of one.
- Every on-disk value is checked once when decoded: a node's sum, level, layout, sorted keys within its bounds, every
  pointer in range and set in the live bitmap, birth generations, value shapes (extents within the disk, not
  overlapping, at most 128 pages); a superblock's fields; a bitmap's sums, zero tails and marks for its own blocks. A
  crafted image gives a named error (or a fallback mount), never a panic.
- An entry never names the root or its own directory (checked when decoded), and is followed only if the inode it
  names records it back (parent, entry offset, kind): no directory handle reaches outside its subtree, and no inode is
  reached by two entries through `lookup`. `readdir` reports the inode and kind entries hold without that check; open
  through `lookup`.
- Trust note: without a walk of every root (scrub, step 41), two inconsistencies cannot be caught before they do harm:
  a bitmap that marks a reachable block free, and an extent pointed at blocks another reference reaches (overwriting
  or truncating it frees them unread). The mutation test leaves exactly these out. An entry's key is not checked
  against its name's hash, so a crafted directory can list a name twice.
- `NoSpace` is decided before anything changes (`reserve`: the operation's data blocks, a bound on the nodes it can
  dirty, and what commit needs), so commit never fails for space. Changes that add also keep back room for an
  `unlink`, which with `truncate` may use it, so a full disk can always be emptied (the floor for snapshot delete
  comes in step 40). After `Io` from a change, any error in the middle of
  one, or a failed `mount`, writes and commits fail with `Io` until a `mount` succeeds; `Io` from `commit` means the
  commit may or may not be durable.
- Performance is the moat: a slowdown is never accepted because it has an explanation (`docs/BENCHMARKS.md`). Disk
  requests per operation are asserted exactly by `block_io_per_operation`.

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p mogfs2`. `tests/fs.rs`: round trip, v1's suite ported (unlink,
  rename, truncate, corruption and fallback, crafted superblocks, `Io` handling, limits, `NoSpace`), stat and times,
  map and verify, colliding names filling a chain, a name in the last hash chain, 255-byte names, the readdir cursor across unlinks, crafted entries,
  the counter check, power cut at every write and flush with subsets of pending writes landing (three workloads, one
  writing nodes out early), the exact I/O table at height 2, 100k entries in one directory (each looked up; listing
  in about one read per leaf; nine in ten unlinked), and a 1 GiB file on a sparse host file with every byte checked
  (about 7 s), and `image.bin` (written at 5b7d427 by a height-2 workload) mounted and rewritten bit for bit.
- `src/tests.rs`: 200 seeds of random changes, commits and remounts with the smallest cache through a disk that panics
  on a write to a block a valid slot reaches, checking the tree and the live bitmap after every step and a fresh
  mount's free space after every commit; and the seeded mutation test (1 to 3 decoded fields changed and resealed up
  to the superblock, then mount and every operation; `MUTATION_SEEDS=n` runs more than the default 1500).
- Benchmark: `cargo bench-host` runs `benches/fs.rs` on criterion; baselines in `docs/BENCHMARKS.md`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
