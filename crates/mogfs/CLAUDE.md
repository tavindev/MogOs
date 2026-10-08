# `crates/mogfs` - MogFS, the checksummed copy-on-write B+tree file system

## What this crate is

The on-disk format (described at the top of `src/lib.rs`) and `Fs<'a, D: Disk>` over memory its caller gives
(`cache_blocks`, `bitmap_words`): `new` (const), in-place `format(seed)` and `mount`, `disk`, `set_time`, `height`,
`lookup`, `readdir` (from an opaque u64 cursor; returns the cursor of the entry its callback stopped at, `u64::MAX` past
the end), `kind`, `stat`, `mkdir`, `create` (opens an existing name), `read`, `write`, `truncate`, `unlink` (`Busy` if the caller's predicate holds the inode it found),
`rename`, `map(file, Page) -> (Block, Sum)` with the free function `verify`, `commit`, and snapshots: `snapshot`
(of the last commit, committing pending changes first), `snapshots` (oldest first), `view(Snapshot) -> View` (its
`lookup`, `readdir`, `stat`, `read` and `map`; no write call) and `delete_snapshot`. It also defines the `Disk`
trait (block numbers stay `u64` there; `Block` converts at that boundary), over `Buf` (one block's bytes), that the
kernel re-exports and the board's `VirtioBlk` implements. Inode, block, page, sum and key offset are
`#[repr(transparent)]` newtypes; a tree key is built from its parts only by `Key::new(Inode, ItemKind, Offset)`.
Phase 7 step 39, in the kernel since step 39b; snapshots since step 40 (the kernel wires them in step 43). All in `src/lib.rs`; `examples/mkfs.rs` writes an empty image through a
file-backed `Disk` with a random seed.

It is **NOT** paths, handles or `..` handling (`crates/kernel`), a page cache, scrub (step 41), or a device driver.

## Responsibilities

- Format, mount (newest valid slot; the older one if the newest's bitmap or rightmost path is corrupt), atomic
  `commit`.
- One copy-on-write B+tree of inode, directory entry and extent items; every pointer holds its child's sum and birth
  generation; data pages are whole 4096-byte blocks whose sums live in their extent (at most 128 pages).
- Free space stored per root as a bitmap: pages of 128 MiB listed in the superblock (up to 128 pages, 16 GiB), or past
  that through an index of blocks listing 255 entries each, the superblock holding its root; after the list a log of
  the words changed since the pages were written, so a small commit writes no bitmap or index block. The format's
  only size bound is the block number width (`MAX_BLOCKS`, 2^62); the caller's memory decides what mounts. The
  other slot's bitmaps are kept reserved.
- Snapshots: an item per snapshot in the live tree (kind 3 under the root inode, keyed by its generation) holds its
  tree root and the root of its bitmap, the live one of that generation (it shares the live list's pages that had not
  changed since they were written and copies the rest, its own blocks marked in it). Pinned, the blocks the live tree
  freed that a snapshot holds, follows the live bitmap in the same page list (one index, one log, one set of dirty
  bits); a release pins a block the newest snapshot's bitmap (held in memory) marks. A block is free only if live,
  pinned and both slots' live and pinned are clear. The snapshots holding a block are a run of consecutive
  generations, so `delete_snapshot` unpins what the deleted one marks and neither neighbour does, reading at most three
  bitmaps (writable clones or restore-from-snapshot, phase 9, would break this). Generations stop at 2^62 - 1, the
  largest key offset.
- Directory entries keyed by a seeded name hash with a collision chain of 8; a full chain is `Collision`. The bound,
  not the hash's strength, is the guarantee (the hash is a seeded multiply-rotate; tests build collisions from the
  seed).
- Inode numbers come from a counter and are never reused; mount checks the counter against the rightmost path.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`.
- Leaf crate: `kernel`, `qemu-virt` and `e2e` (dev) depend on it; it depends on none of them.
- `Fs` is about 22 KiB plus the caller's memory: `cache` (a scratch slot, 32 commit staging slots, a node pool of 32 to
  512 slots, and the page list: its index blocks, or one slot) and `bits` (six bitmaps: live, pinned, their committed
  copies, both slots' reach and the newest snapshot's; and a bit per list page), neither needing to be zeroed. A disk past that memory is `TooBig` at mount. Unit tests (`src/tests.rs`) build the
  crate with small pages (512 blocks), index blocks of 4 entries and an inline list of 2, so their small disks reach
  several index levels; the integration tests use the real sizes.

## Invariants & rules

- Spectre v1: `read`, `map` and `write` index an extent's sums by `(page - off) % EXTENT_MAX`, in bounds by
  construction, since a page from a user's offset, on a mispredicted bound check, reaches past the extent; the kernel
  clamps the offset itself at `dispatch`. A `readdir` cursor indexes nothing: it becomes a key the tree search compares.
- No block reachable from either slot, or from a snapshot either lists, is written. Dirty nodes stay in their cache
  slots; `commit` gives them one free
  run, copies them into the staging slots and writes them in one request, flushes, writes the other slot with the
  log, flushes (`[0, 2, 2]` while a run fits and the staging slots hold them). When the log would overflow, the
  pages changed since they were last written, then the index blocks above them bottom up, join that request ahead of
  the nodes (after their old copies are released, each pointer zeroed so it is released once) and the log empties.
  While the older slot reaches blocks the newest does not (state known at mount and after each commit), a commit
  with no change still writes a superblock (`[0, 1, 1]`), so freed blocks come back without a further change. A
  snapshot's own bitmap blocks are staged ahead of the nodes of the commit that adds its item (`[0, 2, 2]` while the
  staging slots hold them).
  Mount reads blocks 0 to 3 in its first request (the superblocks, and on a fresh image the bitmap page
  and the root, which then cost nothing more), the live index, each live page that is not all zero, and the
  rightmost path, and the newest snapshot's bitmap pages it does not share with the live list; the
  older slot's pages only where it does not share them (same block and sum), rebuilding a shared page's older words
  from the live log's replaced values, and its index one block per level in the staging slots (twice: once for the
  pages, once to check its bitmap marks each index block). When the pool runs short, dirty nodes are written out early (not a commit) at the start of a tree operation,
  never in the middle of one.
- Every on-disk value is checked once when decoded: a node's sum, level, layout, sorted keys within its bounds, every
  pointer in range and set in the live bitmap (or pinned, read through a view), birth generations, value shapes (extents within the disk, not
  overlapping, at most 128 pages; a snapshot item under the root inode only, no newer than the newest snapshot); a superblock's fields and log (words increasing, within the disk, zero tail); a
  bitmap's sums, zero tails and marks for its own blocks. A crafted image gives a named error (or a fallback mount),
  never a panic.
- An entry never names the root or its own directory (checked when decoded), and is followed only if the inode it
  names records it back (parent, entry offset, kind): no directory handle reaches outside its subtree, and no inode is
  reached by two entries through `lookup`. `readdir` reports the inode and kind entries hold without that check; open
  through `lookup`.
- Trust note: without a walk of every root (scrub, step 41), two inconsistencies cannot be caught before they do harm:
  a bitmap that marks a reachable block free, and an extent pointed at blocks another reference reaches (overwriting
  or truncating it frees them unread). The mutation test leaves exactly these out. An entry's key is not checked
  against its name's hash, so a crafted directory can list a name twice.
- Memos skip tree work on repeated access, and must never outlive what they copy: the last two inode items read
  (cleared by `set_inode`, an inode's delete, `mount` and `format`); the data page in `bufs[DATA]`, tagged with its
  block, sum, inode and page (cleared before any rewrite of `bufs[DATA]` and by `release` of its block; a page maps to
  another block only through `write`, which retags it). A `write` leaves its page there unwritten: the block is
  written when the buffer is needed for another page, by `map` of that page, and in `commit` at the head of the nodes'
  request (moved there, if an extent of its own maps it, when the blocks after it are taken); a `release`
  of its block drops it unwritten, and `mount` discards it (the committed state never names an unwritten block). A
  view switches the tree it reads and drops every memo both ways, and `delete_snapshot` evicts clean cache slots
  whose block is neither live nor pinned. The last four lookups that found their entry (cleared before
  `create`, `mkdir`, `unlink` or `rename` change an entry, and by `mount` and `format`); and the leaf the last descent
  reached with `readdir`'s last start index in it (cleared before a cache slot is reused or an insert or delete
  changes the tree). `truncate` and `unlink` take a file of size 0 to have no extents (they end within the size); a
  crafted image that breaks this leaks those blocks (still marked used, never written) until scrub.
- `NoSpace` is decided before anything changes (`reserve`: the operation's data blocks, a bound on the nodes it can
  dirty, and what commit needs), so commit never fails for space. Changes that add (`ADD`) keep back room for an
  `unlink`; `unlink` and `truncate` (`REMOVE`) keep back room for a `delete_snapshot` and its commit (`DELETE`), so a
  full disk can always be emptied, under snapshots too. After `Io` from a change, any error in the middle of
  one, or a failed `mount`, writes and commits fail with `Io` until a `mount` succeeds; `Io` from `commit` means the
  commit may or may not be durable. Fail closed: after an error in the middle of a change, or a failed or partial
  `mount`/`format` (`torn`), reads fail with `Io` too, since the tree in memory may hold a change made halfway; after
  a failed commit, or a failed write of the unwritten data page, reads go on from the tree in memory, which stays
  whole (the page counts as written only once its request has been). `disk_errors_and_full_disks_never_leave_stale_state`
  checks all of this against a model of the files with random request failures and full disks.
- Performance is the moat: a slowdown is never accepted because it has an explanation (`docs/BENCHMARKS.md`). Disk
  requests per operation are asserted exactly by `block_io_per_operation`.

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p mogfs`. `tests/fs.rs`: round trip, v1's suite ported (unlink,
  rename, truncate, corruption and fallback, crafted superblocks, `Io` handling, limits, `NoSpace`), stat and times,
  map and verify, a mount on memory that is not zeroed, colliding names filling a chain, a name in the last hash chain, 255-byte names, the readdir cursor across unlinks, crafted entries,
  the counter check, a 300 GiB sparse file system whose bitmap goes through two index levels (small commits at
  `[_, 2, 2]` with the data page, a 70 MiB write rewriting the index, every file read back after a remount), power cut at every write and flush with subsets of pending writes landing (live files and every
  snapshot's; workloads that change, remove, write nodes out early, take a snapshot of a changed file system, change
  under it and delete it), a snapshot's old contents after overwrites, truncates and unlinks and a remount, its
  `[_, 2, 2]` commit at 1024 and 70000 blocks, deleting two snapshots at `NoSpace` under them, the exact I/O table at height 2, 100k entries in one directory (each looked up; listing
  in about one read per leaf; nine in ten unlinked), and a 1 GiB file on a sparse host file with every byte checked
  (about 7 s), and `image.bin` (written by a height-2 workload, regenerated when the format changed in steps 39b and 40) mounted and rewritten bit for bit.
- `src/tests.rs`: 200 seeds of random changes, commits (half forced to write the pages), snapshots, snapshot deletes
  and remounts with the smallest cache through a disk that panics on a write to a block a valid slot or its snapshots
  reach, each snapshot's files checked through a view against a model and pinned against the snapshots' bitmaps, checking the tree and the live bitmap after every step and a fresh
  mount's free space after every commit; and the seeded mutation test (1 to 3 decoded fields changed and resealed up
  to the superblock, then mount and every operation; `MUTATION_SEEDS=n` runs more than the default 1500). The mutation base holds a snapshot; a
  newest-snapshot field changed below an existing item is left out (mount pays no descent to check it; scrub will).
  Targeted: pages commits on an aged 20000-block disk (every old page and index block freed), a data page moved by
  commit while the pool is short, and snapshot deletes freeing exactly what no other root's bitmap marks (fresh-mount
  free space, no clean cache slot left on a freed block).
- Benchmark: `cargo bench-host` runs `benches/fs.rs` on criterion; baselines in `docs/BENCHMARKS.md`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
