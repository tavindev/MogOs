# Phase 7: Storage that scales

Goal (milestone): MogFS holds a file of at least 1 GiB with snapshots. On the host, a power cut at every write and flush of a workload that writes, snapshots and deletes snapshots always mounts to exactly the old or the new committed generation, with every snapshot intact. In QEMU, a 1 GiB file written through the file API survives `sync`, a snapshot, an overwrite and a QEMU kill at sampled points; scrub then reports a data block the host flipped in the image, naming its file and offset. (A cut at every write is a host test: QEMU can only be killed at sampled points.)

## Steps

Steps 39-41 are pure: MogFS v2 is built as a sibling crate, `crates/mogfs2` (safe, `no_std`, no `alloc`, no dependencies), host-tested on the Mac, so the kernel, board and e2e keep building on v1 and the steps run in parallel with phases 5 and 6. Step 39 fixes the format (root table included); 40 and 41 then run in parallel, both in `crates/mogfs2`. Step 39b is the cutover and the first step that touches the kernel; it ran before 40 and 41, which now land in `crates/mogfs`. Steps 42-45 integrate and wait on phases 5 and 6 (each lists what it needs); 43 and 44 run in parallel after 42, 45 follows 44.

| # | Step | Done when |
| --- | --- | --- |
| 39 | MogFS v2 format (pure, `crates/mogfs2`) | Host: a 1 GiB file round-trips with every byte checked; 100k entries in one directory, each found by `lookup`; names of 255 bytes; deliberately colliding names fill a hash chain to its bound and the next is a named error; the power-cut test at every write and flush and the 200-seed random test pass; a seeded mutation test changes decoded fields, reseals the checksums up to the superblock, then runs mount and every operation (scrub joins in 41): no panic, no write to a block either slot reaches, only `Ok` or a named error. `block_io_per_operation` asserts a small commit at `[0, 2, 2]` on a tree of height 2 or more. |
| 39b | Cutover to v2 | Needs 39-41 and phase 5 step 24. The kernel, board, e2e and `cargo mkfs` move to v2; v1 is deleted and `crates/mogfs2` takes back the name `crates/mogfs`. `readdir` resumes from an opaque u64 cursor (the hash key; `u64::MAX` is past the end) instead of an entry index. Migration is reformat only: the magic changes, and the AGENTS.md one-liner recreates a `disk.img` that does not mount as v2. e2e: `shell_files_survive_a_reboot_only_once_synced` and every other e2e pass on a v2 image; listing 100k entries costs O(n) requests in total; a v1 image prints `fs: Corrupt`. |
| 40 | Snapshots and space reserve (pure, `crates/mogfs`) | Host: a snapshot writes only its entry and its changed bitmap pages, so its commit stays `[0, 2, 2]` whatever the file system's size while the request fits the 32 staging slots; a snapshot reads its old contents after the live tree overwrites, truncates and unlinks them; deleting a snapshot frees exactly the blocks no other root reaches (checked against a fresh mount's bitmaps), and the space returns after delete, commit, commit, including for two fully shared snapshots; the power-cut test at every write covers snapshot create and delete; under a snapshot, unlinking until `NoSpace` still leaves snapshot delete and commit able to succeed. |
| 41 | Scrub (pure, `crates/mogfs`) | Host: scrub visits every block reachable from every root in resumable slices of bounded work, skipping data shared with the snapshot scrubbed before it; a bit flipped in each kind of block (superblock, tree node, bitmap, data) is reported with its block and, for data, its inode and offset; a bitmap that marks a reachable block free is reported; a clean image reports nothing; blocks freed and reused by commits between slices give no report. |
| 42 | Async block path and async file ops | `Disk` takes a batch of scattered requests and virtio-blk keeps them all in flight, completed by interrupt (GIC SPI) instead of polling. `Fs` is owned by a kernel file-system task alone; file ops (`open`, `readdir`, `mkdir`, `unlink`, `rename`, `sync`, read, write) become completion ops it serves, data misses go through `map` so many are in flight at once, and no disk I/O runs with IRQs masked; `sync`s that arrive while a commit is in flight share the next commit (group commit); `io_cancel(token)` returns `Cancelled` (never started) or the op's own result (finished first). e2e: 32 reads in flight beat one in flight by a recorded factor; a cancel before and after completion gives each defined outcome; a `sync` no longer delays a timer tick. |
| 43 | Large files, snapshots and scrub in the kernel (milestone) | A `Volume` handle (init gets it; rights: snapshot, scrub) creates and deletes snapshots and opens one as a read-only `Dir`; scrub runs in bounded slices in the file-system task and reports through the handle; file I/O per op rises from `MAX_BUFFER` to the extent size; `sync` on a file handle commits. e2e: the milestone's QEMU half (1 GiB file, snapshot, overwrite, kill at sampled points, remount shows a committed generation and the snapshot's original bytes, scrub names the flipped block); a process without the `Volume` handle cannot snapshot. |
| 44 | Multi-queue block layer | Per-CPU submission queues feed per-device hardware queues; virtio-blk negotiates `VIRTIO_BLK_F_MQ` with one virtqueue per CPU; no I/O scheduler (`none`). e2e on `-smp 4`: four processes reading in parallel all complete, each request completes on the CPU that submitted it, and IOPS scale over one queue by a recorded factor. |
| 45 | PCIe ECAM and NVMe | PCIe enumerated from the DT; an NVMe driver with an admin queue and one I/O queue pair per CPU, MSI-X through the GICv3 ITS (QEMU `virt` with `gic-version=3,its=on` has no GICv2m, so a minimal ITS driver lands first in this step: command queue, flat device and collection tables, LPI IDs capped at 16 bits; phase 11 step 67 scales it); it implements the same batched `Disk` as virtio-blk, so MogFS is unchanged. e2e: boot with the MogFS disk on `-device nvme`, run the shell test on it; throughput recorded beside virtio-blk. |

### Step details

- **39.** Benchmark (host, `cargo bench-host`): create + 100-byte write + commit and lookup in 400 entries, against v1's 1956 / 784 ns (v2 hashes and copies about three blocks per small commit against v1's one table block, so the number decides whether the allocation log below is needed); new rows: lookup in 100k entries, 1 GiB sequential write + commit and read on the in-memory disk (MiB/s), sequential read after N random 4 KiB overwrites (fragmentation), mount (requests and ns). Invariants: v1's (no block reachable from either slot is written; every on-disk value range-checked once; `NoSpace` decided before anything changes; sticky `Io`), the crafted-image rules below, plus: inode numbers are 64-bit and never reused, so a handle can never reach a later file; a data block is a whole 4096-byte page. Avoids: ext4's rename-without-fsync data loss (M8): the committed state is always a consistent snapshot (a written contract in `src/lib.rs`); copy-on-write data still fragments under random overwrites, as btrfs's does, so the fragmentation row measures it rather than assuming it away.
- **39b.** Needs 39-41 and phase 5 step 24 (it gives the board's `KERNEL` the `Fs` cache memory, which phase 5 restructures). Benchmark (hvf): `test=bench-fs` round trip (136717 ns) and open + close (111 ns), boot with a mounted disk (342 us); the `Fs` static grows with its fixed memory, so the mount cost is measured, not assumed. Invariants: the kernel's `EBUSY`-on-open `unlink` rule stays (never-reused numbers make dropping its scan safe later); `Object` stays 24 bytes and `Call` 56 with a u64 `Inode`. Avoids: `getdents` offsets that skip or repeat entries across `unlink` (the cursor is a key, not a position).
- **40.** Benchmark (host): snapshot create (requests, ns), a 4 KiB overwrite + commit in a snapshotted 1 GiB file, snapshot delete on a 1 GiB disk (requests, ns). Invariants: a block is free only if no root (live, both slots, any snapshot) reaches it; snapshots are read-only; commit never fails for space; a floor below the reserve is open only to snapshot delete and commit. Avoids: btrfs's ENOSPC wedge near full, where unlinks under a snapshot drained the space a delete needed.
  - Plan (for review, revised after the first review). (a) The snapshot list is items in the live tree, kind 3
    under `ROOT` (no extents there, and the rightmost path the inode-counter check reads is unchanged), offset = the
    snapshot's generation `g`. Value (56 bytes): tree root (block, sum, birth generation, level), bitmap root (index
    block, sum, index height). A superblock field holds the newest snapshot's generation (0: none), so mount with no
    snapshot does no extra descent; nonzero, mount checks its item exists, and view, delete and scrub treat a kind-3
    item above it as Corrupt (the mutation test exempts that mutation). The field costs one log entry (the header grows
    to 128 bytes; the root table's 4 zero bytes cannot hold a u64). Decode rules: only under `ROOT`, length 56, `g` below the superblock's generation
    and at most `OFFSET` (mount rejects a generation past `OFFSET`; `snapshot` past it is `TooBig`, since `Key::new`
    only debug-asserts), the root's birth generation at most `g`, level below `MAX_HEIGHT`, blocks in range, index
    height `ix_height(pages)`. Kind-3 items inside a snapshot's or the older slot's tree are inert for views and scrub.
    Decided: items in the tree, not superblock entries (no fixed count, no bitmap-log room taken).
    (b) A snapshot is the last commit `g`: `snapshot()` commits first if anything changed (so it is one call), then
    records `g`'s root and makes S, `g`'s bitmap, in memory from `NEWEST` before the insert, so the insert's
    copy-on-write releases of `g`'s path (and later releases of `g`'s pages) are pinned. Its on-disk bitmap is written
    from S's words: own copies only of the pages changed since the last pages write (bounded by the log), the clean
    pages shared by block, its own index blocks; all its new blocks are pinned at creation and marked in its own
    copies (the claim loop converges as the pages path does). The live bitmap keeps its log; the commit that inserts
    the item writes the nodes, S's copied pages and index blocks in its one request and the superblock: `[0, 2, 2]`
    while the staging slots (32) hold the request, blocks bounded by the log's capacity, not the disk (tested with a
    small log).
    (c) Pinned, as decided: blocks the live tree freed that a snapshot holds. Stored as pages and an index from the
    superblock's reserved field (height max(1, `ix_height`), a zero pointer an all-zero page, marking its own pages and
    index blocks), its changed words in the same superblock log (word index with bit 63 set; the log's ordering and
    `i < words` checks mask the bit), so a commit that frees snapshot-held blocks stays `[0, 2, 2]`. A release tests
    the newest snapshot's bitmap S (in memory). A block is free only if live, pinned and both slots' (live | pinned)
    are clear: `COMMITTED` is computed from `NEWEST` and the slots' pinned kept separately (not folded into `NEWEST`,
    which scrub reads as the committed live bitmap). Pinned's index blocks take `cache_blocks` slots.
    (d) Invariant: the snapshots holding a block are a contiguous run by generation (a block lives from its claim to
    its release, and each snapshot holds what live held at its generation). So deleting S_k unpins exactly
    `S_k & !S_k-1 & !S_k+1`, and the newest `S_k & !S_k-1` (pinned and live are disjoint, so the live term is vacuous;
    testing the committed live bitmap instead would leak a block an uncommitted change released), reading at most three bitmaps' pages
    and skipping pages whose block equals a neighbour's: requests independent of the snapshot count. Phase-9 writable
    clones or restore-from-snapshot break this invariant and must redo delete. Deleting the newest loads the next
    newest into S. Delete evicts clean cache slots whose block is no longer in live | pinned (at most 512), so a
    stale snapshot node never answers a later lookup of its reused block.
    (e) Reading: `Fs::view(id) -> View` with `lookup`, `readdir`, `stat`, `read`, `map` from the snapshot's root and
    its own height; its decode checks pointers and data blocks against live | pinned (`live()` is live only today) and
    an empty root by the view's height; memos keyed by root or cleared on switch, measured; no write method, so
    read-only by construction; `read` and `map` keep the `% EXTENT_MAX` mask. The kernel is untouched (step 43 wires
    the `Volume` handle).
    (f) Reserve: the commit term counts pinned's pages and index too; the floor for unlink and truncate keeps back what
    one `delete_snapshot` plus commits need (one path, both bitmaps' pages and index). Freed blocks leave `COMMITTED`
    only at the commit after next, so a commit with no change still writes a superblock-only generation (`[0, 1, 1]`)
    whenever the older slot holds blocks the newest does not (`COMMITTED != NEWEST | pinned`, state known at mount and
    after every commit, so it survives a remount and also closes the two-commit lag for plain unlinks); done-when asserts the space returns after delete, commit, commit, and a test deletes two snapshots
    sharing everything in a row at `NoSpace`.
    (g) Memory: S, pinned and the older slot's pinned, and pinned's page-dirty bits, through `bitmap_words` (no board
    change). Mount reads pinned's pages for both slots (into `COMMITTED`) and the newest snapshot's, only where they
    differ from live's.
    (h) Tests: the done-when rows; the 200-seed random test grows snapshot create, delete and view reads checked
    against a model of each snapshot, its free-space check counting pinned; the write-panic disk and the mutation
    test's "no write to a block either slot reaches" count every snapshot's reach; the mutation test mutates snapshot
    items and pinned pages; power-cut workloads add create and delete. Benches: as listed, plus mount (requests, ns)
    with 0 and N snapshots, the tracked rows at zero snapshots, and lookup and read through a `View`.
- **41.** Benchmark (host): scrub MiB/s on the in-memory disk (hash-bound), and with one snapshot sharing most of the tree (the skip). Invariants: scrub only reads; each slice does at most a fixed number of requests and re-descends from the current committed root by key, so a cursor never points into freed blocks; a report names the block, never guesses a repair (one copy, no redundancy yet). Avoids: silent corruption that ext4 and XFS cannot detect without separate tooling (dm-integrity); here every byte is covered by a checksum its parent holds.
  - Plan (for review, revised after the first review). (a) API: `Fs::scrub(&mut self, cursor: &mut Scrub,
    requests: usize, report: impl FnMut(Problem)) -> Result<bool, Error>` (true when done). `Problem`: `Corrupt {
    block }` (a superblock, node, index block or bitmap page whose hash fails), `Data { block, inode, offset }`,
    `Unmarked { block }` (its root's bitmap marks a reached block free), `Twice { block }` (reached twice within one
    root). (b) Cursor: fixed positions for the two superblocks, each root's bitmap phase and the older slot; snapshots
    by generation; roots in chronological order (snapshots oldest first, the older slot, live), each a next key and a
    data page within its extent. Each slice re-descends from the root's current committed pointer by key, reading fresh
    from the disk into the staging slots (never the node cache), so a commit between slices never leaves the cursor in
    a freed block; the descent does not count against the budget (a slice reads at most height + `requests` blocks), so
    a budget of 1 still progresses. (c) Sharing: every node is still read and checked under every root, so `Unmarked`
    and `Twice` hold per root; only data pages whose extent sits in a leaf born at or before the root scrubbed before
    (a later one, by the cursor's order) are skipped, since they were read then. Done-when rewords to "skipping data
    shared with the snapshot scrubbed before it". (d) Bitmap check: when a root's walk ends, its seen bits are compared
    with its bitmap, sequentially (the live root against `NEWEST`, the committed live bitmap, since `LIVE` holds
    uncommitted changes; a snapshot and the older slot against their pages, at most one read per page). (e) Reached twice: a seen bitmap per root walk in `Fs` memory (through `bitmap_words`),
    cleared when a root starts; `mark` clears a block's bit (its only setter path: `alloc`, `lead_data`, `claim`), so a
    block claimed by commits between slices never reports. (f) Scrub writes nothing and changes no memo but the seen
    bits. (g) Tests: the done-when rows (a flip in each block kind, the two step-39 inconsistencies built by hand, a
    clean image, slices of one request with commits that free and reuse blocks between them, a snapshot's shared data
    read once: counted requests); the mutation test runs scrub and needs a report or a clean pass, never a panic.
    Bench: scrub MiB/s in memory, and with one snapshot sharing most of the tree. Decided: scrub also walks the older
    slot's tree (a fallback mount uses it).
- **42.** Needs 39b, phase 5 step 24 (lock model) and step 28 (a completion wakes a task on another core). Benchmark (hvf): `test=bench-disk` grows a 4 KiB random-read IOPS row at queue depth 1 and 32, sequential 4 KiB and 256 KiB rows must not regress from 148 / 188 / 3415 / 8629 MiB/s; `test=bench-fs` round trip; the worst IRQ latency during a `sync` (was the whole sync); syscall, yield and pipe within noise. Invariants: rights are checked and buffers pinned at submit; the file-system task holds no authority of its own, only the op it was handed, charged to the submitter's budget (D8); it never waits for an IRQ while holding the big lock (the IRQ handler needs that lock to wake it); every op has exactly one completion, cancelled or not. Avoids: POSIX AIO and `O_NONBLOCK` that does not apply to files, which forced user-space thread pools (M9), and io_uring's privileged workers (D8).
- **43.** Needs 42. If phase 6 steps 35-36 landed first, their cache fills switch to `map` (whole pages, nothing to strip). Benchmark (hvf): 1 GiB sequential write + `sync` and read through the file API (MiB/s), snapshot create latency, scrub MiB/s, and the latency of a 4 KiB write + `sync` while another process streams 1 GiB (commit is whole-file-system; a per-file log tree comes only if this number is bad). Invariants: a snapshot handle is read-only, whatever rights opened it; scrub and snapshot need the `Volume` handle (no ambient authority); a successful `sync` means its generation is durable, an error never clears (M8). Avoids: `CAP_SYS_ADMIN`-style overloaded privilege for snapshots (M6), and the ext3 stall where one `fsync` waits for everyone's data, measured rather than assumed.
- **44.** Needs 42 and phase 5 steps 25 and 28 (SMP bring-up, per-CPU run queues). Benchmark (hvf, `-smp 4`): 4 KiB random-read IOPS at queue depth 32 per core, 1 core against 4; one-core numbers do not regress from 42. Invariants: a request is completed on the CPU that submitted it; no lock is shared between CPUs on the submit path. Avoids: the single-lock request queue blk-mq had to replace, and its I/O scheduler zoo (one scheduler, `none`).
- **45.** Needs 44. Benchmark (hvf): sequential 256 KiB MiB/s and 4 KiB random IOPS per core, NVMe against virtio-blk. Invariants: DMA only into kernel-owned buffers in the identity map, as virtio-blk; a failed or timed-out command is `Io`, never a hang. Avoids: driver-specific block APIs; every device implements one `Disk`.

## MogFS v2 format (decided in step 39)

- One copy-on-write B+tree per root, keyed by (inode, kind, offset or name hash): inode items, directory entries, extents. One CoW, checksum and walk path serves every structure.
- Inode item: kind (file, directory, symlink reserved), mode bits, link count, 64-bit size, mtime, ctime and btime as 64-bit ns (M20). The superblock carries incompatible-feature flags, so later additions (compression, TRIM state) do not force a reformat; an unknown flag refuses the mount.
- Directory entries are keyed by a per-file-system seeded name hash (seed chosen at `mkfs`); a collision chain is bounded and a full one is a named error.
- Tree nodes are 4 KiB blocks ending in v1's checksum of block number and payload; every pointer also carries its child's checksum and birth generation (a Merkle tree), so a lost or misdirected write of a valid old block is caught and scrub can skip shared subtrees.
- Data blocks are whole 4096-byte pages, no trailer: an extent (at most 128 blocks, 512 KiB) carries its blocks' checksums inline. `map(inode, page) -> (block, checksum)` and a verify call let the block layer DMA straight into page-cache frames, with many misses in flight. Ripple for phase 6 step 35: on v1 its fills strip the 8-byte trailer; on v2 they use `map`.
- Commit gives the dirty nodes (bitmap blocks included) consecutive block numbers, computes checksums bottom-up and writes them as one request, then flushes, writes the superblock (which holds the root table: the live tree and the snapshots, each a tree root and an allocation-bitmap root), and flushes: `[0, 2, 2]` at any tree height while a free run fits, one request per run otherwise. Nodes evicted from the cache before commit get their block at eviction. The superblock carries an allocation log (the live bitmap's words changed since its pages were written), so a small commit writes no bitmap block; added in step 39b, when the kernel's `sync` benchmark asked for it.
- Free space is stored, not derived: every root has a copy-on-write allocation bitmap. A block is free only if the live bitmap, both slots' bitmaps (so a fallback mount still finds the older tree intact, as in v1) and the pinned bitmap all have it clear. Pinned holds the blocks the live tree freed that a snapshot still holds: on each free only the newest snapshot's bitmap is tested (a block an older snapshot holds and the live tree still held is in the newest too); snapshot delete rebuilds pinned. Mount reads the superblock and the bitmap blocks (one per 128 MiB of disk), not the whole tree.
- Fixed memory, no `alloc`: the caller gives `Fs` its node cache and bitmap memory, sized for the largest disk it accepts; a larger disk is a named error at mount.
- Space reserve: each change reserves its worst case (tree height, bitmap blocks) before changing anything, as v1's `reserve`; a reserve below that is open to unlink, truncate, snapshot delete and commit, and a floor below it to snapshot delete and commit only.
- 64-bit sizes and block numbers; names up to 255 bytes; inode numbers come from a counter and are never reused.
- Crafted-image rules (each checked once, when decoded): a node records its level and a child's is its parent's minus 1 (no cycles, height capped); keys are sorted and within the parent's bounds; an extent is at most 128 blocks and lies within the disk; one file's extents do not overlap; no birth generation exceeds the superblock's; the inode counter exceeds the largest inode number (read from the rightmost path at mount); whether a node may be rewritten in place comes from in-memory state, never its on-disk birth generation.

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc: survey 39 is step 42, 40 is 39 and 39b, 41 is 40-41, 44 is 44-45 (its M4 and M9 references to "step 39" mean step 42 here). Deferred: change notification (survey 42) and the file-system server channel (survey 43) move to phase 9 beside compat; the shared completion ring (survey 45) is decided after step 42's numbers, under the same 2x-at-batch-32 rule. Renumber if phase 5 or 6 changes length.
- Writable clones (a container's CoW layer) wait for phase 9 containers; with one writable root, pinned changes only on a free and a snapshot delete.
- TRIM, compression, metadata duplication and allocation groups are later; a tracked benchmark has to ask for each.

## What was done

Filled in as each step lands.

- Step 39: `crates/mogfs2` (now `crates/mogfs`, [CLAUDE.md](../../crates/mogfs/CLAUDE.md)) holds the v2 format as decided above, with these concrete choices. Keys are u128 `inode << 64 | kind << 62 | offset` (inode, directory entry, extent). Leaves pack values down from the end; internal entries are (key, block, sum, birth generation), fanout 102, height capped at 8. Leaves and internal nodes under a quarter full merge into a sibling where they fit, and the root collapses. The inode item also records its parent and the offset of the one entry naming it, which `lookup`, `create`, `unlink` and `rename` check, so a crafted entry cannot reach outside its directory or reach an inode twice; it also gives `rename` a parent walk with cycle detection instead of v1's subtree scan. A name's hash chain is 8 slots; a full one is `Collision`. Free space is one bitmap per root: a bitmap index (one block) of 4 KiB pages, each sum covering only the words the disk uses, so at most 255 pages (about 32 GiB; more takes an incompatible feature flag). The superblock's sum covers its header and root table, the index's only its entries; the rest of each block must be zero. Memory comes from the caller: one index slot, the bitmap pages, 32 staging slots and a node pool of 32 to 512 slots, plus three bitmaps. Dirty nodes stay in their slots, tagged in their parents' pointers by slot. A commit gives them, the changed pages and a new index one free run, copies them into the staging slots and writes one request; when the pool runs short, dirty nodes are written out early at the start of a tree operation. The API is v1's plus `format(seed)`, `stat`, `set_time`, `map` with `verify`, `height`, and `readdir` from an opaque u64 cursor. Host tests cover: v1's suite ported; power cut at every write and flush, with subsets of the pending writes landing, over three workloads, one that writes nodes out early; 200 random seeds checking the tree and the live bitmap after every step, through a disk that panics on a write to a reachable block; the I/O table (`[0, 2, 2]` per commit at height 2); colliding names; 255-byte names; the readdir cursor across unlinks; 100k entries (each looked up, listed in about one read per leaf, nine in ten unlinked); and a 1 GiB file on a sparse host file (about 7 s, so not ignored). The seeded mutation test changes one to three decoded fields, reseals up to the superblock, then mounts and runs every operation; it ran clean for 100k seeds. It found and the step fixed two overflow panics, a namespace escape, key aliasing, an out-of-bounds read, and a mount that left an older slot with an unknown feature flag unprotected. Two inconsistencies stay out of reach without a walk of every root, and the test leaves them out: a bitmap that frees a reachable block, and an extent pointed at blocks another reference holds (released unread on overwrite or truncate). Scrub (step 41) should report both, the second as a block reached twice. Benchmarks (`docs/BENCHMARKS.md`): create+write+commit beats v1 on its bench shape (1853-1900 against 1951-1976 ns) and on a 64 MiB disk (1934-1951 against 2076-2098 ns); lookup is 121 ns against 784. The allocation log was not needed. A reviewer pass found that a full disk refused every unlink (changes that add now keep room for one, the spec's lower reserve tier) and that a second `format` on one `Fs` failed; both fixed with tests. Open: when no free run is long enough, choosing a commit's blocks walks every free block (a cost on fragmented large disks, not yet measured); a corrupt data page read before a write changes anything still makes the file system read-only until remount; a rename within a full hash chain returns `Collision`. Fragmentation is measured: a 64 MiB file reads at 12.7 GB/s fresh and 4.9 GB/s after a whole file's worth of random 4 KiB overwrites.
- Step 39b (before 40 and 41, which now land in `crates/mogfs`): a hard cutover with no v1 support, since no disks
  were deployed. `crates/mogfs` (v1) is deleted and the B+tree crate takes its path and name; `cargo mkfs` writes a
  sparse 1 GiB image with a random name-hash seed (`examples/mkfs.rs`), the e2e images use seed 1. `Board::mount`
  sizes the `Fs` memory to the disk (64 node slots, the bitmaps of `min(blocks, MAX_BLOCKS)`) and takes it from
  contiguous frames; the static `Fs` starts with none. `readdir` resumes from the opaque cursor, returned in x1
  (`u64::MAX` past the end; the archive's cursor is an entry index); the cursor left the dispatch clamp, since the tree
  only compares it as a key, and MogFS masks an extent's sum index `% EXTENT_MAX` where v1 masked `% PTRS`. The user
  stub, `ls`, `fsbench` and musl's `getdents` (one native call sized so every name fits as a dirent) carry the cursor;
  musl's `size_of` doubles past 64 KiB. Mount now clears the other slot's bitmap first: memory from the frame
  allocator is not zeroed, and a dirty one underflowed the free count (a host test). Kernel speed: the board
  instantiates `Fs<FsDisk>` at the dev build's opt-level 1, where the first cut ran `open` 2x and `readdir`, `read`
  1.6x v1; a memo of the last two inode items, the data-page memo tagged with its file page, references instead of
  `Item` moves, an early return in `truncate` for an empty file, `inline(always)` byte accessors and mogfs's
  non-generic code at opt-level 3 brought them to `open` +12%, `open(TRUNC)` +6%, `readdir` +7% and `read` -11%
  against v1 (63 hvf rounds); create, mkdir, unlink and rename are 85-97% faster (v1 wrote blocks per change), `ls d1`
  -64%, `mkdir m` -84%, `mv` -77%. e2e: a 200-byte name, an 84 KB file appended past 64 KiB through busybox and
  1000 files in one directory survive a reboot; `shell_files_survive_a_reboot_only_once_synced` and every other e2e
  pass on the B+tree image. Then the slowdowns against v1 were removed:
  (1) `sync-change` wrote four blocks per small commit against v1's two: the live bitmap's page list moved into the
  superblock, followed by a log of the bitmap words changed since the pages were written, so a small commit writes the
  nodes and the superblock; the pages (and the index above them) are written, and the log empties, only when it would
  overflow. Past 128 pages (16 GiB) the superblock holds the root of an index of blocks listing 255 entries each, at
  any height, so the format is bounded only by the block number width (`MAX_BLOCKS`, 2^62); a 300 GiB sparse host test
  runs through two index levels. (2) `compiler_builtins`' `memcpy`/`memmove` assembled each unaligned word from bytes;
  `crates/arch` now provides both (`mem.s`, 16 bytes per `ldp`/`stp`, no FP/SIMD), which also cut `pipe` and `spawn`
  about 20%. (3) `open`, `open(TRUNC)`, `readdir`: a memo of the last four lookups (matched on the name hash first, so a
  miss costs no byte compares), the last leaf reached with its key bounds, and `readdir`'s start index in it.
  (4) `unlink` skips the extent scans for an empty file or a directory; a leaf's values sit in any order, new ones at
  its bottom, so an insert or a remove moves no other value. (5) `write w hello`: a per-request trace showed v1 issuing
  a read and three writes and v2 two writes, the host costing about 35 us for a command's first read request and 70 us
  for its first write; a written data page now stays in its buffer until the buffer is needed, `map` asks for it, or a
  commit writes it at the head of the nodes' request (moved there when the blocks after it are taken), so `write`
  issues no request and a commit with data stays `[0, 2, 2]`. (6) Mount read 3 requests against v1's 2: it reads
  blocks 0 to 3 (the superblocks, and on a fresh image the bitmap page and the root) in one request; 8 blocks measured
  slower at boot. A fault-injection test (random request failures, full disks, a model of the files checked only now
  and then so memos live across changes) found that a commit marked the staged data page written before its request
  (a failed request lost the only copy) and that a failed mount left the old tree readable; both fixed: after an error
  in the middle of a change or a failed mount, every call fails with `Io` until a mount succeeds. The reviewer pass
  found that the data page's move could let an early node write-out take its block, that the move could add log words
  after the commit had decided the log had room, and that a commit writing the pages missed a page its own releases
  changed (leaking the old block, or writing a page over its stale entry); all three fixed, each with a test (the
  random test now forces half its checked commits to write the pages). Final numbers, release profile, against
  `96b22ab` (63 interleaved hvf rounds under the bench lock, `docs/BENCHMARKS.md`): `open`, `open(TRUNC)`, `file-read`
  equal, create, mkdir, unlink, rename and `file-write` 92-98% faster, `pipe` and `spawn` -19..-21%, `ls d1` -53%,
  `write w hello` -83%, `mkdir m` -87%, `mv` -77..-81%, boot 318 -> 291 us, open+write+sync -22%; host lookup -9%,
  create+write+commit -1..-4%. Within this run's noise: `readdir` +1.0% (TCG +37 instructions per call),
  `sync-change` +1.7% median, +0.2% min. `rm m` was +5.9% (min +8.6%) with fewer instructions than v1: the kernel looked the
  name up for its open-handle check and `unlink` searched for it again. `Fs::unlink` now takes the caller's busy
  predicate and searches once (`Busy` maps to `EBUSY`): +3.0% median, +4.8% min, the `unlink` call 51 counter ticks
  median against v1's 38. Open, for the coordinator: the rest is cold memory, 40 distinct cache lines in three nodes
  (66 key probes over 10 node visits, and a 1200-byte item-array shift in the root directory's leaf) against about 11
  in v1 (eight 56-byte entries in one directory block and two records). Not done: a
  v1 image is not tested (its magic differs, so it mounts as `Corrupt`); the kernel never calls `set_time`, so inode
  times read 0; `unlink` or truncate of a large file does its extent deletes with IRQs masked until step 42. The first
  reviewer pass found the `getdents` re-list could skip or repeat an entry when a directory changed between its two
  calls (fixed: one call), `size_of` bisected every doubled range (fixed), and the unbounded large-file unlink
  (documented in the kernel contract).
- Step 40: snapshots and the space reserve, in `crates/mogfs` (the kernel wires them in step 43). `snapshot()` commits
  pending changes, then records that commit `g` as an item in the live tree (kind 3 under the root inode, offset `g`):
  `g`'s tree root and the root of the snapshot's own bitmap, `g`'s live bitmap. That bitmap shares the live list's
  pages that had not changed since they were written and has its own copies of the others, of the pages its own
  blocks land in, and its own index; those blocks are staged ahead of the commit's nodes (`[0, 2, 2]` while the
  staging slots hold them; 1 GiB: `[0, 2, 2]`). `snapshots()` lists them, `view(Snapshot)` reads one (`lookup`,
  `readdir`, `stat`, `read`, `map`; no write call), `delete_snapshot` removes one (1 GiB, with its commit: `[10, 2,
  2]`). One deviation from the reviewed plan: pinned (the blocks the live tree freed that a snapshot holds) is not a
  page list of its own but the second half of the live one (one index, one log, one set of dirty bits), so the pages
  path covers it unchanged and an all-zero pinned page is a zero pointer; the superblock's reserved pinned-index
  field stays zero. A release pins a block the newest snapshot's bitmap (in memory) marks; pinned blocks are also in
  the map of blocks either slot reaches, so the free test still reads two maps, and an unpinned block waits out the
  commit after next like a freed one. A commit with no change writes a superblock-only generation (`[0, 1, 1]`) while
  the older slot reaches blocks the newest does not, state known at mount and after each commit. Delete unpins what
  the snapshot marks and neither neighbour does (the snapshots holding a block are a run of consecutive generations),
  reading at most three bitmaps, finds the neighbours by a seek and a step back, and evicts clean cache slots whose
  block is neither live nor pinned. `reserve` has three levels: a change that adds keeps room for an unlink, which
  keeps room for a snapshot delete and its commit. Pinned's memory is not even zeroed until a slot holds pinned words
  or a snapshot exists. Generations stop at 2^62 - 1, the largest key offset (mount rejects more, commit refuses to
  pass it). Tests: the done-when rows (old contents after overwrites, truncates, unlinks, churn and a remount; the
  commit's requests at 1024 and 70000 blocks; delete frees exactly what no other root's bitmap marks, against a fresh
  mount, also with 120 snapshots spanning leaves; power cut at every write and flush across a snapshot of a changed
  file system, changes under it and its delete; two fully shared snapshots deleted at `NoSpace` under them, space
  back after delete, commit, commit); the random test takes and deletes snapshots and checks each through a view
  against a model and pinned against the snapshots' bitmaps; the mutation test's base holds a snapshot (20k seeds in
  debug clean); the fault test takes and deletes snapshots under failing requests. Reviews: the first found the
  newest snapshot's bitmap reloaded at run time through mount's shortcut (blocks a snapshot held could be freed; now
  read from the disk), the second that deleting the last snapshot left its bitmap in memory during the item's delete
  (a merge could pin a block forever) and that a crafted item at offset 0 passed decode; all fixed with tests. Found
  on the way: a new snapshot's own blocks were taken as those pinned and in its bitmap, which also matched the nodes
  its item's insert released. A security audit of main's 39b code in the middle of this step found an older slot's
  marks past a smaller disk's size counted as used (free underflowed: a debug-build panic), fixed on main (`12e4e5f`)
  and here. Numbers (release kernels, 63 interleaved hvf rounds with an A/A first, load 5-11): every call and shell
  command within its A/A spread against `ddcbacd`; boot within noise over six runs (medians -21 to +10 us; a timer
  around mount: 1297 against 1332 ticks median, 989 against 897 min). Host (11 rounds, A/A within 0.6%): mount of a
  1 GiB file -33%, create+write+commit, 1 GiB read and write within noise; lookup +1.7..2.7% in three runs while the
  kernel's lookup path runs the same TCG instruction count (`open` 619 both) and `open` is -1.0% on hvf: host codegen,
  open for the coordinator. New rows: snapshot create 10.9 us, delete + commit 20.7 us, a 4 KiB overwrite + commit
  under a snapshot 10.4 us, mount with 8 snapshots 14.1 us, 400 lookups through a view 58.7 us (all 1 GiB file).
