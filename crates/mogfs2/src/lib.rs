//! MogFS v2: a checksummed copy-on-write B+tree file system over 4 KiB blocks.
//!
//! Format (little-endian):
//! - Blocks 0 and 1 are superblock slots; generation `g` goes to slot `g % 2`, and mount takes the valid slot with
//!   the highest generation. Superblock: magic u64, generation u64, block count u64, incompatible features u64 (none
//!   are known yet; a set bit refuses the mount), next inode u64, name-hash seed u64, pinned bitmap index (block u64,
//!   sum u64; zero until snapshots), root count u32 (1: the live root), 4 zero bytes, then each root: tree root (block
//!   u64, sum u64, birth generation u64, level u64) and bitmap index (block u64, sum u64).
//! - Superblocks, tree nodes and bitmap indexes end in a 64-bit hash of their block number and first 4088 bytes, and
//!   every pointer to one holds that hash (a node pointer also its birth generation), so a lost or misdirected write
//!   of a valid old block reads as corrupt.
//! - Data and bitmap pages are whole 4096-byte blocks; the hash of each (block number and page) lives in the extent
//!   or index pointing at it.
//! - Free space is stored: a root's bitmap index holds (block, sum) for each 128 MiB page of the disk (block 0: an
//!   all-zero page); bit `b % 64` of u64 word `b / 64` is set if the root reaches block `b`: the superblocks, its tree
//!   nodes and data, its bitmap pages and index.
//! - Tree: one B+tree keyed by u128 `inode << 64 | kind << 62 | offset`. Node header: level u8 (0 = leaf), zero u8,
//!   count u16, 4 zero bytes. Leaf: `count` items (key u128, value offset u16, value length u16), values packed down
//!   from byte 4088 in item order. Internal: `count` entries (key u128, block u64, sum u64, birth generation u64);
//!   child `i` holds keys from entry `i`'s (the node's own lower bound for `i = 0`) up to entry `i + 1`'s.
//! - Items. Inode (kind 0, offset 0): kind u8 (1 file, 2 directory, 3 symlink: reserved), zero u8, mode u16, links
//!   u32, size u64, parent u64, entry u64 (the offset of the entry naming it in `parent`), mtime, ctime and btime u64
//!   (ns). Directory entry (kind 1; offset: the seeded hash of the name, low 3 bits the slot in its collision chain of
//!   8): inode u64 (never the root or the directory itself), kind u8, name; it is followed only if that inode records
//!   it back (parent, entry and kind). Extent (kind 2; offset: its first page): first block u64, then each of its 1
//!   to 128 pages' sums.
//! - Inode numbers come from the superblock's counter and are never reused.
//! - Copy-on-write: no block reachable from either slot is written. Data pages are written at once to free blocks
//!   (or over one written since the last commit). `commit` gives the dirty nodes, the changed bitmap pages and a new
//!   index consecutive free blocks, writes them in one request, flushes, writes the other slot, and flushes.
//!   Contract: the committed state is always a consistent snapshot of the file system as of a `commit` call.
#![cfg_attr(not(test), no_std)]

use core::cmp::min;
use core::slice::{from_mut, from_ref};

pub const BLOCK_SIZE: usize = 4096;
pub type Block = [u8; BLOCK_SIZE];
pub const NAME_MAX: usize = 255;
pub const MAX_FILE_SIZE: u64 = 1 << 52;
/// Pages an extent maps at most.
pub const EXTENT_MAX: u64 = 128;
/// Largest file system, in blocks (about 32 GiB); `format` uses at most this much of a bigger disk.
pub const MAX_BLOCKS: u64 = MAX_PAGES as u64 * PAGE_BITS;
/// Fewest node cache slots `Fs` accepts.
pub const MIN_POOL: usize = 32;
/// The root directory.
pub const ROOT: Inode = Inode(0);

const PAGE_BITS: u64 = 8 * BLOCK_SIZE as u64;
const PAGE_WORDS: usize = BLOCK_SIZE / 8;
const MAX_PAGES: usize = (BLOCK_SIZE - 8) / 16;
const MAX_POOL: usize = 512;
/// Cache slots that stage the dirty nodes of a commit request, after the index and the bitmap pages.
const STAGE: usize = 32;
const MAX_CACHE: usize = 1 + MAX_PAGES + STAGE + MAX_POOL;
/// Superblock bytes its sum covers: the header and a root table of one root.
const SB_LEN: usize = 120;
const MIN_BLOCKS: u64 = 16;
const MAX_HEIGHT: usize = 8;
const MAGIC: u64 = u64::from_le_bytes(*b"MogFS\0\0\x02");
const CHAIN: u64 = 8;

const END: usize = BLOCK_SIZE - 8;
const HDR: usize = 8;
const ITEM: usize = 20;
const ENTRY: usize = 40;
const CAP: usize = END - HDR;
const FANOUT: usize = CAP / ENTRY;
const QUARTER: usize = CAP / 4;
const INODE_LEN: usize = 56;
const EXTENT_ITEM: usize = ITEM + 8 + 8 * EXTENT_MAX as usize;

const INODE: u64 = 0;
const DIRENT: u64 = 1;
const EXTENT: u64 = 2;
const OFFSET: u64 = (1 << 62) - 1;
const FILE: u8 = 1;
const DIR: u8 = 2;

/// A pointer's block with this bit set names the dirty node in that cache slot.
const TAG: u64 = 1 << 63;
const EMPTY: u64 = u64::MAX;
const NONE: u128 = u128::MAX;

const LIVE: usize = 0;
const NEWEST: usize = 1;
const COMMITTED: usize = 2;
const DATA: usize = 0;
const META: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    Corrupt,
    NotFound,
    Exists,
    NotDir,
    IsDir,
    InvalidName,
    /// A file past `MAX_FILE_SIZE`, a tree past its height limit, or a disk past the memory `Fs` was given.
    TooBig,
    NoSpace,
    NotEmpty,
    /// Every slot of the name's hash chain in the directory is taken.
    Collision,
    /// The newest superblock needs a feature this version does not know.
    Unsupported,
}

/// A block device of 4 KiB blocks; a request covers `bufs.len()` consecutive blocks from `block`.
pub trait Disk {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error>;
    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error>;
    /// Returns once every completed write is durable.
    fn flush(&mut self) -> Result<(), Error>;
    fn blocks(&self) -> u64;
}

/// A file or directory; its number is never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mode: u16,
    pub links: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub btime: u64,
}

/// Cache blocks `Fs` needs for a disk of `blocks` blocks with `pool` node slots (at least `MIN_POOL`).
pub const fn cache_blocks(blocks: u64, pool: usize) -> usize {
    1 + pages(blocks) + STAGE + pool
}

/// Bitmap words `Fs` needs for a disk of `blocks` blocks.
pub const fn bitmap_words(blocks: u64) -> usize {
    3 * pages(blocks) * PAGE_WORDS
}

/// Whether `page` read from `block` matches the sum `map` gave for it.
pub fn verify(block: u64, page: &Block, sum: u64) -> Result<(), Error> {
    if checksum(block, page) == sum {
        Ok(())
    } else {
        Err(Error::Corrupt)
    }
}

const fn pages(blocks: u64) -> usize {
    let b = if blocks < MAX_BLOCKS {
        blocks
    } else {
        MAX_BLOCKS
    };
    b.div_ceil(PAGE_BITS) as usize
}

#[derive(Clone, Copy)]
struct Ptr {
    block: u64,
    sum: u64,
    generation: u64,
}

#[derive(Clone, Copy)]
struct Item {
    kind: u8,
    mode: u16,
    links: u32,
    size: u64,
    parent: u64,
    /// The offset of the one directory entry that names it.
    entry: u64,
    mtime: u64,
    ctime: u64,
    btime: u64,
}

struct Super {
    flags: u64,
    valid: bool,
    generation: u64,
    blocks: u64,
    next_inode: u64,
    seed: u64,
    root: Ptr,
    level: usize,
    index: (u64, u64),
}

/// A directory entry's offset, inode and kind.
type Entry = (u64, Inode, u8);

/// The dirty nodes from the root down to a leaf: slot, child index taken, and key bounds at each level.
#[derive(Default)]
struct Path {
    slot: [usize; MAX_HEIGHT],
    idx: [usize; MAX_HEIGHT],
    lo: [u128; MAX_HEIGHT],
    hi: [u128; MAX_HEIGHT],
}

/// A file system on `D` with memory its caller gives: `cache` (bitmap staging and node slots, `cache_blocks`) and
/// `bits` (`bitmap_words`). `Io` from any change, or a failed `mount`, leaves it refusing writes and commits until a
/// `mount` succeeds; after `Io` from `commit`, durability is unknown.
pub struct Fs<'a, D> {
    disk: D,
    cache: &'a mut [Block],
    bits: &'a mut [u64],
    blocks: u64,
    pages: usize,
    words: usize,
    generation: u64,
    next_inode: u64,
    seed: u64,
    now: u64,
    root: Ptr,
    height: usize,
    /// The live root's bitmap index as of the last commit; its entries stay in `cache[0]`.
    index: (u64, u64),
    /// Bitmap pages changed since the last commit.
    dirty: [u64; 4],
    /// The live bitmap's words changed since the last commit (`lo..hi`), and those the last commit changed.
    span: (usize, usize),
    prev_span: (usize, usize),
    changed: bool,
    free: u64,
    /// Every block below it is in use.
    hint: u64,
    /// `cache[0]` holds the live bitmap index, `cache[1..base]` stage commits, `cache[base..top]` cache nodes.
    base: usize,
    top: usize,
    /// Each cache slot's block (`EMPTY` if none or not yet given), whether it holds a dirty node, and last use.
    blk: [u64; MAX_CACHE],
    dirt: [bool; MAX_CACHE],
    ndirty: usize,
    stamp: [u64; MAX_CACHE],
    clock: u64,
    /// `DATA` (a data page) and `META` (superblocks, and scratch for an extent's value).
    bufs: [Block; 2],
    /// The block and sum `bufs[DATA]` holds unchanged.
    cached: Option<(u64, u64)>,
    broken: bool,
}

impl<'a, D: Disk> Fs<'a, D> {
    /// An unmounted file system; `mount` or `format` it before use.
    pub const fn new(disk: D, cache: &'a mut [Block], bits: &'a mut [u64]) -> Self {
        Self {
            disk,
            cache,
            bits,
            blocks: 0,
            pages: 0,
            words: 0,
            generation: 0,
            next_inode: 0,
            seed: 0,
            now: 0,
            root: Ptr {
                block: 0,
                sum: 0,
                generation: 0,
            },
            height: 1,
            index: (0, 0),
            dirty: [0; 4],
            span: (usize::MAX, 0),
            prev_span: (usize::MAX, 0),
            changed: false,
            free: 0,
            hint: 0,
            base: 0,
            top: 0,
            blk: [EMPTY; MAX_CACHE],
            dirt: [false; MAX_CACHE],
            ndirty: 0,
            stamp: [0; MAX_CACHE],
            clock: 0,
            bufs: [[0; BLOCK_SIZE]; 2],
            cached: None,
            broken: true,
        }
    }

    /// The disk; replace it only before `mount`.
    pub fn disk(&mut self) -> &mut D {
        &mut self.disk
    }

    /// The time (ns) that changes from now on record.
    pub fn set_time(&mut self, ns: u64) {
        self.now = ns;
    }

    /// Levels in the tree (1: the root is a leaf).
    pub fn height(&self) -> usize {
        self.height
    }

    /// Writes an empty file system (only a root directory) whose name hash uses `seed`, and commits it.
    pub fn format(&mut self, seed: u64) -> Result<(), Error> {
        self.setup(min(self.disk.blocks(), MAX_BLOCKS))?;
        if self.blocks < MIN_BLOCKS {
            return Err(Error::NoSpace);
        }
        self.broken = false;
        // Stale superblocks could outrank the new ones.
        self.bufs[META].fill(0);
        for slot in 0..2 {
            let r = self.disk.write(slot, from_ref(&self.bufs[META]));
            self.broken |= r.is_err();
            r?;
        }
        self.flush()?;
        self.bits.fill(0);
        for map in [LIVE, NEWEST, COMMITTED] {
            self.bits[map * self.words] = 0b11;
        }
        self.free = self.blocks - 2;
        self.cache[0].fill(0);
        (self.generation, self.next_inode, self.seed) = (0, 1, seed);
        let s = self.new_node(0);
        self.root = tagged(s);
        self.height = 1;
        let root = Item {
            kind: DIR,
            mode: 0o755,
            links: 1,
            size: 0,
            parent: 0,
            entry: 0,
            mtime: self.now,
            ctime: self.now,
            btime: self.now,
        };
        let (s, at) = self.insert(key(0, INODE, 0), INODE_LEN)?;
        encode(&mut self.cache[s][at..at + INODE_LEN], &root);
        self.commit()
    }

    /// Loads the newest valid slot, or the older one if the newest's bitmap or rightmost path is corrupt; drops
    /// uncommitted changes. Blocks the other slot's bitmap marks stay reserved.
    pub fn mount(&mut self) -> Result<(), Error> {
        self.broken = true;
        self.cached = None;
        self.disk.read(0, &mut self.bufs)?;
        let disk = min(self.disk.blocks(), MAX_BLOCKS);
        let mut slots = [
            superblock(&self.bufs[0], 0, disk),
            superblock(&self.bufs[1], 1, disk),
        ];
        if slots[0].as_ref().map(|s| s.generation) < slots[1].as_ref().map(|s| s.generation) {
            slots.swap(0, 1);
        }
        for i in 0..2 {
            let Some(s) = &slots[i] else { continue };
            if s.flags != 0 {
                if i == 0 {
                    return Err(Error::Unsupported);
                }
                continue;
            }
            if !s.valid {
                continue;
            }
            self.setup(s.blocks)?;
            let r = self.load_bitmap(s, LIVE).and_then(|()| {
                (self.root, self.height) = (s.root, s.level + 1);
                (self.generation, self.next_inode) = (s.generation, s.next_inode);
                self.seed = s.seed;
                self.check_counter()
            });
            match r {
                Ok(()) => {}
                Err(Error::Corrupt) => continue,
                Err(e) => return Err(e),
            }
            // The other slot's blocks stay reserved while its bitmap holds, even if its tree failed above.
            let w = self.words;
            if let Some(o) = &slots[1 - i]
                && o.valid
                && let Err(e) = self.load_bitmap(o, COMMITTED)
            {
                self.bits[COMMITTED * w..3 * w].fill(0);
                if e != Error::Corrupt {
                    return Err(e);
                }
            }
            self.bits.copy_within(..w, NEWEST * w);
            for i in 0..w {
                self.bits[COMMITTED * w + i] |= self.bits[i];
            }
            self.index = s.index;
            self.dirty = [0; 4];
            self.prev_span = (0, w);
            self.changed = false;
            let used: u64 = (0..w)
                .map(|i| (self.bits[i] | self.bits[COMMITTED * w + i]).count_ones() as u64)
                .sum();
            self.free = self.blocks - used;
            self.hint = 0;
            self.broken = false;
            return Ok(());
        }
        Err(Error::Corrupt)
    }

    pub fn lookup(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.dir(dir)?;
        let e = self.find_entry(dir.0, name)?.0.ok_or(Error::NotFound)?;
        self.child(dir.0, e)?;
        Ok(e.1)
    }

    /// Calls `f` with each entry's name, inode and kind from `cursor` on (0: the first), in hash order, until `f`
    /// returns true; returns the cursor of that entry, which a later call starts at, or `u64::MAX` past the end.
    pub fn readdir(
        &mut self,
        dir: Inode,
        cursor: u64,
        mut f: impl FnMut(&[u8], Inode, Kind) -> bool,
    ) -> Result<u64, Error> {
        self.dir(dir)?;
        if cursor > OFFSET {
            return Ok(u64::MAX);
        }
        let mut k = key(dir.0, DIRENT, cursor);
        let end = key(dir.0, EXTENT, 0);
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                let k = ikey(n, i);
                if k >= end {
                    return Ok(u64::MAX);
                }
                let v = value(n, i);
                if f(&v[9..], Inode(le64(v, 0)), kind_of(v[8])) {
                    return Ok(k as u64 & OFFSET);
                }
            }
            if hi >= end {
                return Ok(u64::MAX);
            }
            k = hi;
        }
    }

    pub fn kind(&mut self, inode: Inode) -> Result<Kind, Error> {
        Ok(kind_of(self.inode(inode.0)?.kind))
    }

    pub fn stat(&mut self, inode: Inode) -> Result<Stat, Error> {
        let it = self.inode(inode.0)?;
        Ok(Stat {
            kind: kind_of(it.kind),
            size: it.size,
            mode: it.mode,
            links: it.links,
            mtime: it.mtime,
            ctime: it.ctime,
            btime: it.btime,
        })
    }

    pub fn mkdir(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        self.add(dir, name, DIR)
    }

    /// The inode `name` names, or a new empty file if it names none.
    pub fn create(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        self.add(dir, name, FILE)
    }

    /// Reads from `offset` up to the end of the file; returns the byte count (0 at or past the end).
    pub fn read(&mut self, file: Inode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let it = self.file(file.0)?;
        let end = min(it.size, offset.saturating_add(buf.len() as u64));
        let mut pos = offset;
        while pos < end {
            let (page, at) = (pos / BLOCK_SIZE as u64, (pos % BLOCK_SIZE as u64) as usize);
            let n = min(BLOCK_SIZE - at, (end - pos) as usize);
            let out = &mut buf[(pos - offset) as usize..][..n];
            match self.extent_at(file.0, page)? {
                None => out.fill(0),
                Some((off, start, _)) => {
                    let j = page - off;
                    self.load_page(start + j, le64(&self.bufs[META], 8 + 8 * j as usize))?;
                    out.copy_from_slice(&self.bufs[DATA][at..at + n]);
                }
            }
            pos += n as u64;
        }
        Ok(end.saturating_sub(offset) as usize)
    }

    /// The block holding `page` of `file` and that page's sum (check it with `verify`), or `None` for a hole.
    pub fn map(&mut self, file: Inode, page: u64) -> Result<Option<(u64, u64)>, Error> {
        self.file(file.0)?;
        if page >= MAX_FILE_SIZE / BLOCK_SIZE as u64 {
            return Ok(None);
        }
        Ok(self.extent_at(file.0, page)?.map(|(off, start, _)| {
            let j = page - off;
            (start + j, le64(&self.bufs[META], 8 + 8 * j as usize))
        }))
    }

    /// Writes `data` at `offset`, growing the file; a gap past the old end reads as zeros. `NoSpace` changes nothing.
    pub fn write(&mut self, file: Inode, offset: u64, data: &[u8]) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let mut it = self.file(file.0)?;
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&e| e <= MAX_FILE_SIZE)
            .ok_or(Error::TooBig)?;
        if data.is_empty() {
            return Ok(());
        }
        let pages = (end - 1) / BLOCK_SIZE as u64 - offset / BLOCK_SIZE as u64 + 1;
        // Each page may add an extent or a sum; the extents at both ends may split.
        let bytes = (pages as usize + 2) * (ITEM + 16) * 2 + 2 * EXTENT_ITEM;
        self.reserve(pages, 3, bytes)?;
        let r = self.write_pages(file.0, offset, data).and_then(|()| {
            it.size = it.size.max(end);
            (it.mtime, it.ctime) = (self.now, self.now);
            self.set_inode(file.0, &it)
        });
        self.broken |= r.is_err();
        r
    }

    /// Empties `file`.
    pub fn truncate(&mut self, file: Inode) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let mut it = self.file(file.0)?;
        let bytes = self.extent_bytes(file.0)?;
        if it.size == 0 && bytes == 0 {
            return Ok(());
        }
        self.reserve(0, 2, bytes)?;
        let r = self.remove_extents(file.0).and_then(|()| {
            it.size = 0;
            (it.mtime, it.ctime) = (self.now, self.now);
            self.set_inode(file.0, &it)
        });
        self.broken |= r.is_err();
        r
    }

    /// Removes a file or an empty directory with its blocks. `NoSpace` changes nothing.
    pub fn unlink(&mut self, dir: Inode, name: &[u8]) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let mut d = self.dir(dir)?;
        let e = self.find_entry(dir.0, name)?.0.ok_or(Error::NotFound)?;
        let (off, inode, _) = e;
        let it = self.child(dir.0, e)?;
        if it.kind == DIR
            && let Some((s, i, _)) = self.seek(key(inode.0, DIRENT, 0))?
            && ikey(&self.cache[s], i) < key(inode.0, EXTENT, 0)
        {
            return Err(Error::NotEmpty);
        }
        let bytes = self.extent_bytes(inode.0)?;
        self.reserve(0, 4, bytes)?;
        let r = self
            .delete(key(dir.0, DIRENT, off))
            .and_then(|()| self.remove_extents(inode.0))
            .and_then(|()| self.delete(key(inode.0, INODE, 0)))
            .and_then(|()| {
                (d.mtime, d.ctime) = (self.now, self.now);
                self.set_inode(dir.0, &d)
            });
        self.broken |= r.is_err();
        r
    }

    /// Moves an entry, possibly to another directory; `Exists` if `to_name` is taken, `InvalidName` if a directory would
    /// move into itself or below itself; renaming an entry to itself does nothing. `NoSpace` changes nothing.
    pub fn rename(
        &mut self,
        from_dir: Inode,
        from_name: &[u8],
        to_dir: Inode,
        to_name: &[u8],
    ) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        if !valid_name(to_name) || !valid_name(from_name) {
            return Err(Error::InvalidName);
        }
        let mut from = self.dir(from_dir)?;
        let e = self
            .find_entry(from_dir.0, from_name)?
            .0
            .ok_or(Error::NotFound)?;
        let (off, inode, kind) = e;
        let mut it = self.child(from_dir.0, e)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        let mut to = self.dir(to_dir)?;
        let (taken, slot) = self.find_entry(to_dir.0, to_name)?;
        if taken.is_some() {
            return Err(Error::Exists);
        }
        let slot = slot.ok_or(Error::Collision)?;
        if from_dir != to_dir && kind == DIR && self.below(inode.0, to_dir.0)? {
            return Err(Error::InvalidName);
        }
        self.reserve(0, 5, 0)?;
        let r = self
            .delete(key(from_dir.0, DIRENT, off))
            .and_then(|()| self.put_entry(to_dir.0, slot, inode.0, kind, to_name))
            .and_then(|()| {
                (it.ctime, it.parent, it.entry) = (self.now, to_dir.0, slot);
                self.set_inode(inode.0, &it)?;
                (from.mtime, from.ctime) = (self.now, self.now);
                self.set_inode(from_dir.0, &from)?;
                if to_dir != from_dir {
                    (to.mtime, to.ctime) = (self.now, self.now);
                    self.set_inode(to_dir.0, &to)?;
                }
                Ok(())
            });
        self.broken |= r.is_err();
        r
    }

    /// Makes every change so far durable, atomically; does nothing if nothing changed.
    pub fn commit(&mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        if !self.changed {
            return Ok(());
        }
        let generation = self.generation.checked_add(1).ok_or(Error::Corrupt)?;
        let r = self.write_commit(generation);
        self.broken |= r.is_err();
        r
    }

    fn write_commit(&mut self, generation: u64) -> Result<(), Error> {
        if self.index.0 != 0 {
            self.release(self.index.0)?;
        }
        // Release the old copy of each page that changes, then find blocks for the index, the pages and the nodes;
        // claiming those blocks may change more pages, so repeat until it does not.
        let mut done = [0u64; 4];
        let start = loop {
            while let Some(p) = (0..self.pages).find(|&p| bit(&self.dirty, p) && !bit(&done, p)) {
                done[p / 64] |= 1 << (p % 64);
                let b = le64(&self.cache[0], 16 * p);
                if b != 0 {
                    self.release(b)?;
                }
            }
            let k = 1 + count_set(&self.dirty) + self.ndirty;
            if (self.free as usize) < k {
                return Err(Error::NoSpace);
            }
            let start = self.start(k);
            let (mut b, mut more) = (start, false);
            for _ in 0..k {
                b = self.next_free(b);
                let p = (b / PAGE_BITS) as usize;
                more |= !bit(&self.dirty, p);
                self.dirty[p / 64] |= 1 << (p % 64);
                b += 1;
            }
            if !more {
                break start;
            }
        };
        let changed = count_set(&self.dirty);
        let mut b = start;
        for s in 0..=changed {
            self.blk[s] = self.claim(&mut b);
        }
        for s in self.base..self.top {
            if self.dirt[s] {
                self.blk[s] = self.claim(&mut b);
            }
        }
        self.finalize(generation);
        let words = self.blocks.div_ceil(64) as usize;
        for (s, p) in (1..).zip((0..self.pages).filter(|&p| bit(&self.dirty, p))) {
            let len = 8 * min(PAGE_WORDS, words - p * PAGE_WORDS);
            let page = &mut self.cache[s];
            for (i, word) in self.bits[p * PAGE_WORDS..][..len / 8].iter().enumerate() {
                page[8 * i..8 * i + 8].copy_from_slice(&word.to_le_bytes());
            }
            page[len..].fill(0);
            let sum = checksum(self.blk[s], &page[..len]);
            let (b, entry) = (self.blk[s], &mut self.cache[0][16 * p..16 * p + 16]);
            entry[..8].copy_from_slice(&b.to_le_bytes());
            entry[8..].copy_from_slice(&sum.to_le_bytes());
        }
        self.index = (
            self.blk[0],
            seal(self.blk[0], &mut self.cache[0], 16 * self.pages),
        );
        self.write_out(0, changed + 1)?;
        self.flush()?;
        let sb = &mut self.bufs[META];
        sb.fill(0);
        let fields = [
            MAGIC,
            generation,
            self.blocks,
            0,
            self.next_inode,
            self.seed,
            0,
            0,
            1,
            self.root.block,
            self.root.sum,
            self.root.generation,
            self.height as u64 - 1,
            self.index.0,
            self.index.1,
        ];
        for (i, f) in fields.iter().enumerate() {
            sb[8 * i..8 * i + 8].copy_from_slice(&f.to_le_bytes());
        }
        seal(generation % 2, sb, SB_LEN);
        let r = self.disk.write(generation % 2, from_ref(sb));
        self.broken |= r.is_err();
        r?;
        self.flush()?;
        self.generation = generation;
        self.clean();
        // Elsewhere the live, newest and older bitmaps agree, so the committed one (newest | older) is unchanged.
        let w = self.words;
        for i in min(self.span.0, self.prev_span.0)..self.span.1.max(self.prev_span.1) {
            let (l, n, c) = (
                self.bits[i],
                self.bits[NEWEST * w + i],
                &mut self.bits[2 * w + i],
            );
            let before = (l | *c).count_ones();
            *c = n | l;
            self.free = self.free + before as u64 - (l | *c).count_ones() as u64;
            self.bits[NEWEST * w + i] = l;
        }
        (self.prev_span, self.span) = (self.span, (usize::MAX, 0));
        self.dirty = [0; 4];
        self.hint = 0;
        self.changed = false;
        Ok(())
    }

    /// Sizes the memory for a disk of `blocks` and drops every cached node.
    fn setup(&mut self, blocks: u64) -> Result<(), Error> {
        self.broken = true;
        let pages = pages(blocks);
        let base = 1 + pages + STAGE;
        let pool = min(self.cache.len().saturating_sub(base), MAX_POOL);
        if pool < MIN_POOL || self.bits.len() < bitmap_words(blocks) {
            return Err(Error::TooBig);
        }
        (self.blocks, self.pages, self.words) = (blocks, pages, pages * PAGE_WORDS);
        (self.base, self.top) = (base, base + pool);
        self.blk.fill(EMPTY);
        self.dirt.fill(false);
        self.ndirty = 0;
        self.dirty = [0; 4];
        (self.span, self.prev_span) = ((usize::MAX, 0), (0, pages * PAGE_WORDS));
        self.cached = None;
        Ok(())
    }

    /// Reads `s`'s bitmap into `map` (the words of this disk's size), its index into `cache[0]` for `LIVE` or else
    /// `bufs[DATA]`; skips pages the live index shares with it. The bitmap must mark its own blocks and the root.
    fn load_bitmap(&mut self, s: &Super, map: usize) -> Result<(), Error> {
        let live = map == LIVE;
        let (ix, sum) = s.index;
        let buf = if live {
            &mut self.cache[0]
        } else {
            &mut self.bufs[DATA]
        };
        self.disk.read(ix, from_mut(buf))?;
        let (pages, words) = (pages(s.blocks), s.blocks.div_ceil(64) as usize);
        let len = 16 * pages;
        if le64(buf, END) != sum
            || checksum(ix, &buf[..len]) != sum
            || buf[len..END].iter().any(|&b| b != 0)
        {
            return Err(Error::Corrupt);
        }
        let w = self.words;
        self.bits[map * w..(map + 1) * w].fill(0);
        for p in 0..pages {
            let buf = if live {
                &self.cache[0]
            } else {
                &self.bufs[DATA]
            };
            let (b, sum) = (le64(buf, 16 * p), le64(buf, 16 * p + 8));
            if b == 0 && sum == 0 {
                continue;
            }
            if !(2..s.blocks).contains(&b) {
                return Err(Error::Corrupt);
            }
            if !live && p < self.pages && le64(&self.cache[0], 16 * p) == b {
                continue;
            }
            self.disk.read(b, from_mut(&mut self.cache[1]))?;
            let (page, n) = (&self.cache[1], min(PAGE_WORDS, words - p * PAGE_WORDS));
            let tail = le64(page, 8 * (n - 1)) >> (s.blocks % 64);
            if checksum(b, &page[..8 * n]) != sum
                || page[8 * n..].iter().any(|&b| b != 0)
                || (p == pages - 1 && !s.blocks.is_multiple_of(64) && tail != 0)
            {
                return Err(Error::Corrupt);
            }
            for (i, word) in page[..8 * n].as_chunks::<8>().0.iter().enumerate() {
                if p * PAGE_WORDS + i < w {
                    self.bits[map * w + p * PAGE_WORDS + i] = u64::from_le_bytes(*word);
                }
            }
        }
        // Pages shared with the live index were skipped: their bits are the live ones. Blocks past this disk's size
        // (the other slot may claim more) are never allocated, so need no check.
        let has = |b: u64| {
            let i = (b / 64) as usize;
            b >= self.blocks
                || (self.bits[map * w + i] | if live { 0 } else { self.bits[i] }) >> (b % 64) & 1
                    != 0
        };
        let buf = if live {
            &self.cache[0]
        } else {
            &self.bufs[DATA]
        };
        let pages_held = (0..pages).all(|p| {
            let b = le64(buf, 16 * p);
            b == 0 || has(b)
        });
        if !(has(0) && has(1) && has(ix) && has(s.root.block) && pages_held) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// The counter exceeds every inode number: the last item down the rightmost path has the largest.
    fn check_counter(&mut self) -> Result<(), Error> {
        let (mut p, mut level, mut lo) = (self.root, self.height - 1, 0);
        loop {
            let s = self.node(p, level, lo, NONE)?;
            let n = &self.cache[s];
            let c = count(n);
            if level == 0 {
                if c > 0 && (ikey(n, c - 1) >> 64) as u64 >= self.next_inode {
                    return Err(Error::Corrupt);
                }
                return Ok(());
            }
            if c > 1 {
                lo = ekey(n, c - 1);
            }
            (p, level) = (eptr(n, c - 1), level - 1);
        }
    }

    fn inode(&mut self, inode: u64) -> Result<Item, Error> {
        let k = key(inode, INODE, 0);
        let (s, _) = self.leaf(k)?;
        let n = &self.cache[s];
        let i = search(n, k);
        if i == count(n) || ikey(n, i) != k {
            return Err(Error::NotFound);
        }
        Ok(decode(value(n, i)))
    }

    fn file(&mut self, inode: u64) -> Result<Item, Error> {
        let it = self.inode(inode)?;
        if it.kind == DIR {
            return Err(Error::IsDir);
        }
        Ok(it)
    }

    fn dir(&mut self, inode: Inode) -> Result<Item, Error> {
        let it = self.inode(inode.0)?;
        if it.kind != DIR {
            return Err(Error::NotDir);
        }
        Ok(it)
    }

    /// The inode entry `e` of `dir` names, if it records that entry back; `Corrupt` if not.
    fn child(&mut self, dir: u64, e: Entry) -> Result<Item, Error> {
        let it = self.inode(e.1.0).map_err(|_| Error::Corrupt)?;
        if it.parent != dir || it.entry != e.0 || it.kind != e.2 {
            return Err(Error::Corrupt);
        }
        Ok(it)
    }

    fn set_inode(&mut self, inode: u64, it: &Item) -> Result<(), Error> {
        let (s, at) = self.value_mut(key(inode, INODE, 0))?;
        encode(&mut self.cache[s][at..at + INODE_LEN], it);
        Ok(())
    }

    /// `name`'s entry in `dir` (its offset, inode and kind), and the first free offset in its hash chain.
    fn find_entry(&mut self, dir: u64, name: &[u8]) -> Result<(Option<Entry>, Option<u64>), Error> {
        let base = name_hash(self.seed, name);
        let (mut k, end) = (key(dir, DIRENT, base), key(dir, DIRENT, base + CHAIN));
        let mut used = 0u8;
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                let ik = ikey(n, i);
                if ik >= end {
                    break;
                }
                let v = value(n, i);
                if &v[9..] == name {
                    return Ok((Some((ik as u64 & OFFSET, Inode(le64(v, 0)), v[8])), None));
                }
                used |= 1 << ((ik as u64 & OFFSET) - base);
            }
            if hi >= end {
                break;
            }
            k = hi;
        }
        let free = (used != 0xff).then(|| base + used.trailing_ones() as u64);
        Ok((None, free))
    }

    fn add(&mut self, dir: Inode, name: &[u8], kind: u8) -> Result<Inode, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let mut d = self.dir(dir)?;
        let (found, slot) = self.find_entry(dir.0, name)?;
        match found {
            Some(e) if kind == FILE => return self.child(dir.0, e).map(|_| e.1),
            Some(_) => return Err(Error::Exists),
            None => {}
        }
        if self.broken {
            return Err(Error::Io);
        }
        let slot = slot.ok_or(Error::Collision)?;
        let inode = self.next_inode;
        let next = inode.checked_add(1).ok_or(Error::NoSpace)?;
        self.reserve(0, 3, ITEM + 9 + name.len() + ITEM + INODE_LEN)?;
        let it = Item {
            kind,
            mode: if kind == DIR { 0o755 } else { 0o644 },
            links: 1,
            size: 0,
            parent: dir.0,
            entry: slot,
            mtime: self.now,
            ctime: self.now,
            btime: self.now,
        };
        self.next_inode = next;
        let r = self
            .put_entry(dir.0, slot, inode, kind, name)
            .and_then(|()| self.insert(key(inode, INODE, 0), INODE_LEN))
            .and_then(|(s, at)| {
                encode(&mut self.cache[s][at..at + INODE_LEN], &it);
                (d.mtime, d.ctime) = (self.now, self.now);
                self.set_inode(dir.0, &d)
            });
        self.broken |= r.is_err();
        r.map(|()| Inode(inode))
    }

    fn put_entry(
        &mut self,
        dir: u64,
        off: u64,
        inode: u64,
        kind: u8,
        name: &[u8],
    ) -> Result<(), Error> {
        let (s, at) = self.insert(key(dir, DIRENT, off), 9 + name.len())?;
        let v = &mut self.cache[s][at..at + 9 + name.len()];
        v[..8].copy_from_slice(&inode.to_le_bytes());
        v[8] = kind;
        v[9..].copy_from_slice(name);
        Ok(())
    }

    /// Whether `target` is `dir` or below it, walking up parents; a parent cycle is `Corrupt` (Brent's algorithm).
    fn below(&mut self, dir: u64, target: u64) -> Result<bool, Error> {
        let (mut hare, mut tortoise, mut power, mut steps) = (target, target, 1u64, 0u64);
        loop {
            if hare == dir {
                return Ok(true);
            }
            if hare == ROOT.0 {
                return Ok(false);
            }
            hare = self.inode(hare)?.parent;
            if hare == tortoise {
                return Err(Error::Corrupt);
            }
            steps += 1;
            if steps == power {
                (tortoise, power, steps) = (hare, power * 2, 0);
            }
        }
    }

    fn write_pages(&mut self, inode: u64, offset: u64, data: &[u8]) -> Result<(), Error> {
        let end = offset + data.len() as u64;
        let mut pos = offset;
        while pos < end {
            let (page, at) = (pos / BLOCK_SIZE as u64, (pos % BLOCK_SIZE as u64) as usize);
            let n = min(BLOCK_SIZE - at, (end - pos) as usize);
            let old = self.extent_at(inode, page)?;
            let old_block = old.map(|(off, start, _)| start + page - off);
            match old {
                Some((off, start, _)) if n < BLOCK_SIZE => {
                    let sum = le64(&self.bufs[META], 8 + 8 * (page - off) as usize);
                    self.load_page(start + page - off, sum)?;
                }
                _ => self.bufs[DATA].fill(0),
            }
            self.cached = None;
            self.bufs[DATA][at..at + n].copy_from_slice(&data[(pos - offset) as usize..][..n]);
            let b = match old_block {
                Some(b) if !self.has(COMMITTED, b) => b,
                _ => self.alloc()?,
            };
            let sum = checksum(b, &self.bufs[DATA]);
            let r = self.disk.write(b, from_ref(&self.bufs[DATA]));
            self.broken |= r.is_err();
            r?;
            self.cached = Some((b, sum));
            self.set_page(inode, page, b, sum, old)?;
            pos += n as u64;
        }
        Ok(())
    }

    /// Maps `page` of `inode` to block `b` with `sum`; `old` is the extent that covered it, its value in scratch.
    fn set_page(
        &mut self,
        inode: u64,
        page: u64,
        b: u64,
        sum: u64,
        old: Option<(u64, u64, u64)>,
    ) -> Result<(), Error> {
        if let Some((off, start, c)) = old {
            let j = page - off;
            if start + j == b {
                let (s, at) = self.value_mut(key(inode, EXTENT, off))?;
                self.cache[s][at + 8 + 8 * j as usize..][..8].copy_from_slice(&sum.to_le_bytes());
                return Ok(());
            }
            self.release(start + j)?;
            self.delete(key(inode, EXTENT, off))?;
            let j = j as usize;
            if j + 1 < c as usize {
                // The tail's value: its first block over the replaced page's sum, then the sums after it.
                self.bufs[META][8 + 8 * j..16 + 8 * j]
                    .copy_from_slice(&(start + j as u64 + 1).to_le_bytes());
                self.put_item(key(inode, EXTENT, page + 1), 8 + 8 * j, 8 + 8 * c as usize)?;
            }
            if j > 0 {
                self.put_item(key(inode, EXTENT, off), 0, 8 + 8 * j)?;
            }
        }
        if page > 0
            && let Some((off, start, c)) = self.extent_at(inode, page - 1)?
            && off + c == page
            && start + c == b
            && c < EXTENT_MAX
        {
            let c = c as usize;
            self.bufs[META][8 + 8 * c..16 + 8 * c].copy_from_slice(&sum.to_le_bytes());
            self.delete(key(inode, EXTENT, off))?;
            return self.put_item(key(inode, EXTENT, off), 0, 16 + 8 * c);
        }
        self.bufs[META][..8].copy_from_slice(&b.to_le_bytes());
        self.bufs[META][8..16].copy_from_slice(&sum.to_le_bytes());
        self.put_item(key(inode, EXTENT, page), 0, 16)
    }

    /// Inserts an item whose value is `bufs[META][from..to]`.
    fn put_item(&mut self, k: u128, from: usize, to: usize) -> Result<(), Error> {
        let (s, at) = self.insert(k, to - from)?;
        self.cache[s][at..at + to - from].copy_from_slice(&self.bufs[META][from..to]);
        Ok(())
    }

    /// The extent of `inode` covering `page` (first page, first block, pages), its value copied to `bufs[META]`.
    fn extent_at(&mut self, inode: u64, page: u64) -> Result<Option<(u64, u64, u64)>, Error> {
        let mut k = key(inode, EXTENT, page.saturating_sub(EXTENT_MAX - 1));
        let last = key(inode, EXTENT, page);
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                if ikey(n, i) > last {
                    return Ok(None);
                }
                let (off, v) = (ikey(n, i) as u64 & OFFSET, value(n, i));
                let c = (v.len() as u64 - 8) / 8;
                if page < off + c {
                    self.bufs[META][..v.len()].copy_from_slice(v);
                    return Ok(Some((off, le64(v, 0), c)));
                }
            }
            if hi > last {
                return Ok(None);
            }
            k = hi;
        }
    }

    /// Bytes of `inode`'s extent items.
    fn extent_bytes(&mut self, inode: u64) -> Result<usize, Error> {
        let (mut k, end) = (key(inode, EXTENT, 0), key(inode, EXTENT, OFFSET) + 1);
        let mut bytes = 0;
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                if ikey(n, i) >= end {
                    return Ok(bytes);
                }
                bytes += ITEM + value(n, i).len();
            }
            if hi >= end {
                return Ok(bytes);
            }
            k = hi;
        }
    }

    fn remove_extents(&mut self, inode: u64) -> Result<(), Error> {
        let (first, end) = (key(inode, EXTENT, 0), key(inode, EXTENT, OFFSET) + 1);
        while let Some((s, i, _)) = self.seek(first)? {
            let n = &self.cache[s];
            let k = ikey(n, i);
            if k >= end {
                break;
            }
            let v = value(n, i);
            let (start, c) = (le64(v, 0), (v.len() as u64 - 8) / 8);
            for b in start..start + c {
                self.release(b)?;
            }
            self.delete(k)?;
        }
        Ok(())
    }

    /// Fails with `NoSpace` unless `data` blocks, the nodes `paths` tree changes touching `bytes` of items may dirty
    /// or add, and what commit needs (every dirty node, the bitmap pages and an index) are free.
    fn reserve(&self, data: u64, paths: usize, bytes: usize) -> Result<(), Error> {
        // Adjacent leaves together hold at least a quarter leaf, and internal nodes are at least a quarter full.
        let leaves = 2 * bytes.div_ceil(QUARTER);
        let nodes = 2 * (paths * (self.height + 1) + leaves);
        let need = data + (nodes + self.ndirty + self.pages + 1) as u64;
        if need > self.free {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    fn has(&self, map: usize, b: u64) -> bool {
        self.bits[map * self.words + (b / 64) as usize] >> (b % 64) & 1 != 0
    }

    fn used(&self, b: u64) -> bool {
        self.has(LIVE, b) || self.has(COMMITTED, b)
    }

    /// A block in range that the live tree reaches.
    fn live(&self, b: u64) -> bool {
        (2..self.blocks).contains(&b) && self.has(LIVE, b)
    }

    fn mark(&mut self, b: u64) {
        self.bits[(b / 64) as usize] |= 1 << (b % 64);
        self.touch(b);
        self.free -= 1;
        self.changed = true;
    }

    /// Notes that `b`'s live bit changed.
    fn touch(&mut self, b: u64) {
        let (p, i) = ((b / PAGE_BITS) as usize, (b / 64) as usize);
        self.dirty[p / 64] |= 1 << (p % 64);
        self.span = (min(self.span.0, i), self.span.1.max(i + 1));
    }

    fn alloc(&mut self) -> Result<u64, Error> {
        let b = self.next_free(self.hint);
        if b >= self.blocks {
            return Err(Error::NoSpace);
        }
        self.mark(b);
        self.hint = b + 1;
        Ok(b)
    }

    /// The first free block at or after `b` (`blocks` if none).
    fn next_free(&self, mut b: u64) -> u64 {
        let w = self.words;
        while b < self.blocks {
            let i = (b / 64) as usize;
            let used = (self.bits[i] | self.bits[COMMITTED * w + i]) >> (b % 64);
            if used == !0 >> (b % 64) {
                b = (b | 63) + 1;
            } else {
                return b + used.trailing_ones() as u64;
            }
        }
        self.blocks
    }

    /// The start of the first run of `k` free blocks, or of the first free block if no run is that long.
    fn start(&self, k: usize) -> u64 {
        let first = self.next_free(self.hint);
        let mut b = first;
        while b < self.blocks {
            let mut end = b;
            while end < self.blocks && end - b < k as u64 && !self.used(end) {
                end += 1;
            }
            if end - b == k as u64 {
                return b;
            }
            b = self.next_free(end);
        }
        first
    }

    /// `b` left the live tree: free at once if no slot reaches it, else once the commit after next replaces them.
    fn release(&mut self, b: u64) -> Result<(), Error> {
        if !self.live(b) {
            return Err(Error::Corrupt);
        }
        self.bits[(b / 64) as usize] &= !(1 << (b % 64));
        self.touch(b);
        if !self.has(COMMITTED, b) {
            self.free += 1;
            self.hint = min(self.hint, b);
        }
        if self.cached.is_some_and(|(c, _)| c == b) {
            self.cached = None;
        }
        self.changed = true;
        Ok(())
    }

    fn load_page(&mut self, b: u64, sum: u64) -> Result<(), Error> {
        if self.cached == Some((b, sum)) {
            return Ok(());
        }
        self.cached = None;
        self.disk.read(b, from_mut(&mut self.bufs[DATA]))?;
        verify(b, &self.bufs[DATA], sum)?;
        self.cached = Some((b, sum));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        let r = self.disk.flush();
        self.broken |= r.is_err();
        r
    }

    /// The cache slot holding the node `p` points to at `level`, read and checked against the bounds if not cached.
    fn node(&mut self, p: Ptr, level: usize, lo: u128, hi: u128) -> Result<usize, Error> {
        self.clock += 1;
        if p.block & TAG != 0 {
            return Ok((p.block & !TAG) as usize);
        }
        if let Some(s) = (self.base..self.top).find(|&s| self.blk[s] == p.block) {
            if self.cache[s][0] as usize != level {
                return Err(Error::Corrupt);
            }
            self.stamp[s] = self.clock;
            return Ok(s);
        }
        let s = self.victim();
        self.blk[s] = EMPTY;
        self.disk.read(p.block, from_mut(&mut self.cache[s]))?;
        self.check(s, p, level, lo, hi)?;
        (self.blk[s], self.stamp[s]) = (p.block, self.clock);
        Ok(s)
    }

    /// A slot without a dirty node: an empty one, else the least recently used.
    fn victim(&self) -> usize {
        (self.base..self.top)
            .filter(|&s| !self.dirt[s])
            .min_by_key(|&s| match self.blk[s] {
                EMPTY => 0,
                _ => self.stamp[s] + 1,
            })
            .unwrap_or(self.base)
    }

    /// Checks a node just read into slot `s`: its sum, level, layout, keys within `lo..hi`, and values.
    fn check(&self, s: usize, p: Ptr, level: usize, lo: u128, hi: u128) -> Result<(), Error> {
        let n = &self.cache[s];
        let c = count(n);
        let bad = le64(n, END) != p.sum
            || checksum(p.block, &n[..END]) != p.sum
            || n[0] as usize != level
            || n[1] != 0
            || n[4..HDR] != [0; 4];
        if bad {
            return Err(Error::Corrupt);
        }
        let mut prev = None;
        if level > 0 {
            if c == 0 || c > FANOUT {
                return Err(Error::Corrupt);
            }
            for i in 0..c {
                let (k, e) = (ekey(n, i), eptr(n, i));
                if k < lo || k >= hi || prev.is_some_and(|q| k <= q) {
                    return Err(Error::Corrupt);
                }
                if e.generation > p.generation || !self.live(e.block) {
                    return Err(Error::Corrupt);
                }
                prev = Some(k);
            }
            return Ok(());
        }
        let root = level == self.height - 1 && lo == 0 && hi == NONE;
        if c * ITEM > CAP || (c == 0 && !root) {
            return Err(Error::Corrupt);
        }
        let (mut top, mut prev_end) = (END, None);
        for i in 0..c {
            let (k, off, len) = (ikey(n, i), voff(n, i), vlen(n, i));
            if off + len != top || off < HDR + ITEM * c {
                return Err(Error::Corrupt);
            }
            top = off;
            if k < lo || k >= hi || prev.is_some_and(|q| k <= q) {
                return Err(Error::Corrupt);
            }
            prev = Some(k);
            let (inode, kind, o) = ((k >> 64) as u64, (k as u64) >> 62, k as u64 & OFFSET);
            let v = &n[off..off + len];
            let ok = match kind {
                INODE => {
                    o == 0
                        && len == INODE_LEN
                        && matches!(v[0], FILE | DIR)
                        && v[1] == 0
                        && le16(v, 2) <= 0o7777
                        && le64(v, 8) <= MAX_FILE_SIZE
                        && le64(v, 24) <= OFFSET
                }
                DIRENT => {
                    (10..=9 + NAME_MAX).contains(&len)
                        && le64(v, 0) != ROOT.0
                        && le64(v, 0) != inode
                        && matches!(v[8], FILE | DIR)
                        && valid_name(&v[9..])
                }
                EXTENT => {
                    let pages = (len as u64).saturating_sub(8) / 8;
                    let ok = len % 8 == 0
                        && (1..=EXTENT_MAX).contains(&pages)
                        && o + pages <= MAX_FILE_SIZE / BLOCK_SIZE as u64
                        && prev_end.is_none_or(|e| e <= k)
                        && key(inode, EXTENT, o + pages - 1) < hi
                        && le64(v, 0) < self.blocks
                        && (le64(v, 0)..le64(v, 0) + pages).all(|b| self.live(b));
                    prev_end = Some(key(inode, EXTENT, o + pages));
                    ok
                }
                _ => false,
            };
            if !ok {
                return Err(Error::Corrupt);
            }
            if kind != EXTENT {
                prev_end = None;
            }
        }
        Ok(())
    }

    /// The leaf whose range holds `k`, and the leaf's upper bound (`NONE` for the last), without changing anything.
    fn leaf(&mut self, k: u128) -> Result<(usize, u128), Error> {
        let (mut p, mut level, mut lo, mut hi) = (self.root, self.height - 1, 0, NONE);
        loop {
            let s = self.node(p, level, lo, hi)?;
            if level == 0 {
                return Ok((s, hi));
            }
            let n = &self.cache[s];
            let i = route(n, k);
            if i > 0 {
                lo = ekey(n, i);
            }
            if i + 1 < count(n) {
                hi = ekey(n, i + 1);
            }
            (p, level) = (eptr(n, i), level - 1);
        }
    }

    /// The first item at or after `k`: its slot, index and its leaf's upper bound.
    fn seek(&mut self, mut k: u128) -> Result<Option<(usize, usize, u128)>, Error> {
        loop {
            let (s, hi) = self.leaf(k)?;
            let i = search(&self.cache[s], k);
            if i < count(&self.cache[s]) {
                return Ok(Some((s, i, hi)));
            }
            if hi == NONE {
                return Ok(None);
            }
            k = hi;
        }
    }

    /// Makes the path to `k`'s leaf dirty, writing dirty nodes out first if the cache is short of slots.
    fn cow(&mut self, k: u128) -> Result<Path, Error> {
        if self.top - self.base - self.ndirty < 3 * (self.height + 2) {
            self.spill()?;
        }
        let mut path = Path::default();
        let (mut p, mut level, mut lo, mut hi) = (self.root, self.height - 1, 0, NONE);
        let mut parent: Option<(usize, usize)> = None;
        loop {
            let s = self.node(p, level, lo, hi)?;
            self.make_dirty(s)?;
            match parent {
                None => self.root = tagged(s),
                Some((ps, i)) => set_eblock(&mut self.cache[ps], i, TAG | s as u64),
            }
            (path.slot[level], path.lo[level], path.hi[level]) = (s, lo, hi);
            if level == 0 {
                return Ok(path);
            }
            let n = &self.cache[s];
            let i = route(n, k);
            path.idx[level] = i;
            if i > 0 {
                lo = ekey(n, i);
            }
            if i + 1 < count(n) {
                hi = ekey(n, i + 1);
            }
            (p, parent, level) = (eptr(n, i), Some((s, i)), level - 1);
        }
    }

    /// Marks the node in slot `s` dirty, releasing its block if it was clean.
    fn make_dirty(&mut self, s: usize) -> Result<(), Error> {
        if !self.dirt[s] {
            self.release(self.blk[s])?;
            (self.blk[s], self.dirt[s]) = (EMPTY, true);
            self.ndirty += 1;
        }
        Ok(())
    }

    /// A new, empty dirty node at `level`.
    fn new_node(&mut self, level: usize) -> usize {
        let s = self.victim();
        self.cache[s][..HDR].copy_from_slice(&[level as u8, 0, 0, 0, 0, 0, 0, 0]);
        (self.blk[s], self.dirt[s]) = (EMPTY, true);
        self.ndirty += 1;
        self.changed = true;
        s
    }

    /// Frees node slot `s` and the block of the node it held.
    fn drop_node(&mut self, s: usize) -> Result<(), Error> {
        if self.dirt[s] {
            self.dirt[s] = false;
            self.ndirty -= 1;
        } else {
            self.release(self.blk[s])?;
        }
        self.blk[s] = EMPTY;
        Ok(())
    }

    /// The slot and value offset of item `k`, on a dirty path.
    fn value_mut(&mut self, k: u128) -> Result<(usize, usize), Error> {
        let s = self.cow(k)?.slot[0];
        let n = &self.cache[s];
        let i = search(n, k);
        if i == count(n) || ikey(n, i) != k {
            return Err(Error::Corrupt);
        }
        Ok((s, voff(n, i)))
    }

    /// Adds item `k` with a `len`-byte value to fill; returns its slot and value offset.
    fn insert(&mut self, k: u128, len: usize) -> Result<(usize, usize), Error> {
        let path = self.cow(k)?;
        let s = path.slot[0];
        let i = search(&self.cache[s], k);
        if i < count(&self.cache[s]) && ikey(&self.cache[s], i) == k {
            return Err(Error::Corrupt);
        }
        if used(&self.cache[s]) + ITEM + len <= CAP {
            return Ok((s, leaf_insert(&mut self.cache[s], i, k, len)));
        }
        // Split so that the larger half, counting the new item at `i`, is as small as it can be.
        let n = &self.cache[s];
        let c = count(n);
        let size = |j: usize| match j.cmp(&i) {
            core::cmp::Ordering::Less => ITEM + vlen(n, j),
            core::cmp::Ordering::Equal => ITEM + len,
            core::cmp::Ordering::Greater => ITEM + vlen(n, j - 1),
        };
        let total = used(n) + ITEM + len;
        let (mut left, mut m, mut best) = (0, 1, usize::MAX);
        for j in 0..c {
            left += size(j);
            let worst = left.max(total - left);
            if worst < best {
                (best, m) = (worst, j + 1);
            }
        }
        let r = self.new_node(0);
        let cut = if m <= i { m } else { m - 1 };
        let (a, b) = pair(self.cache, s, r);
        leaf_move(a, cut, b);
        let (slot, at) = if m <= i {
            (r, leaf_insert(&mut self.cache[r], i - cut, k, len))
        } else {
            (s, leaf_insert(&mut self.cache[s], i, k, len))
        };
        self.add_child(&path, 1, ikey(&self.cache[r], 0), r)?;
        Ok((slot, at))
    }

    /// Adds entry (`k`, dirty `child`) at `level`, after the path's entry there; splits up to a new root as needed.
    fn add_child(
        &mut self,
        path: &Path,
        mut level: usize,
        mut k: u128,
        mut child: usize,
    ) -> Result<(), Error> {
        loop {
            if level == self.height {
                if self.height == MAX_HEIGHT {
                    return Err(Error::TooBig);
                }
                let r = self.new_node(level);
                let old = path.slot[level - 1];
                internal_insert(&mut self.cache[r], 0, 0, tagged(old));
                internal_insert(&mut self.cache[r], 1, k, tagged(child));
                (self.root, self.height) = (tagged(r), self.height + 1);
                return Ok(());
            }
            let (s, i) = (path.slot[level], path.idx[level] + 1);
            if count(&self.cache[s]) < FANOUT {
                internal_insert(&mut self.cache[s], i, k, tagged(child));
                return Ok(());
            }
            let r = self.new_node(level);
            let half = FANOUT / 2;
            let (a, b) = pair(self.cache, s, r);
            internal_move(a, half, b);
            if i <= half {
                internal_insert(&mut self.cache[s], i, k, tagged(child));
            } else {
                internal_insert(&mut self.cache[r], i - half, k, tagged(child));
            }
            (k, child, level) = (ekey(&self.cache[r], 0), r, level + 1);
        }
    }

    /// Removes item `k`, merging nodes that fall below a quarter full into a sibling where they fit.
    fn delete(&mut self, k: u128) -> Result<(), Error> {
        let path = self.cow(k)?;
        let s = path.slot[0];
        let i = search(&self.cache[s], k);
        if i == count(&self.cache[s]) || ikey(&self.cache[s], i) != k {
            return Err(Error::Corrupt);
        }
        leaf_remove(&mut self.cache[s], i);
        for level in 0..self.height - 1 {
            let s = path.slot[level];
            if used(&self.cache[s]) >= QUARTER {
                break;
            }
            let (parent, i) = (path.slot[level + 1], path.idx[level + 1]);
            if count(&self.cache[s]) == 0 {
                self.drop_node(s)?;
                internal_remove(&mut self.cache[parent], i);
                continue;
            }
            let pc = count(&self.cache[parent]);
            let j = if i + 1 < pc {
                i + 1
            } else if i > 0 {
                i - 1
            } else {
                break;
            };
            let lo = if j == 0 {
                path.lo[level + 1]
            } else {
                ekey(&self.cache[parent], j)
            };
            let hi = if j + 1 < pc {
                ekey(&self.cache[parent], j + 1)
            } else {
                path.hi[level + 1]
            };
            let sib = self.node(eptr(&self.cache[parent], j), level, lo, hi)?;
            if used(&self.cache[s]) + used(&self.cache[sib]) > CAP {
                break;
            }
            let (l, r, ri) = if j > i {
                (s, sib, j)
            } else {
                self.make_dirty(sib)?;
                set_eblock(&mut self.cache[parent], j, TAG | sib as u64);
                (sib, s, i)
            };
            let at = count(&self.cache[l]);
            let (a, b) = pair(self.cache, r, l);
            if level == 0 {
                leaf_move(a, 0, b);
            } else {
                internal_move(a, 0, b);
                let sep = ekey(&self.cache[parent], ri);
                self.cache[l][HDR + ENTRY * at..][..16].copy_from_slice(&sep.to_le_bytes());
            }
            self.drop_node(r)?;
            internal_remove(&mut self.cache[parent], ri);
        }
        while self.height > 1 && self.root.block & TAG != 0 {
            let r = (self.root.block & !TAG) as usize;
            match count(&self.cache[r]) {
                0 => {
                    self.cache[r][..HDR].fill(0);
                    self.height = 1;
                }
                1 => {
                    self.root = eptr(&self.cache[r], 0);
                    self.drop_node(r)?;
                    self.height -= 1;
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// Writes every dirty node out (not a commit), leaving them clean.
    fn spill(&mut self) -> Result<(), Error> {
        let generation = self.generation.checked_add(1).ok_or(Error::Corrupt)?;
        if (self.free as usize) < self.ndirty {
            self.broken = true;
            return Err(Error::NoSpace);
        }
        let mut b = self.start(self.ndirty);
        for s in self.base..self.top {
            if self.dirt[s] {
                self.blk[s] = self.claim(&mut b);
            }
        }
        self.finalize(generation);
        self.write_out(1, 1)?;
        self.clean();
        Ok(())
    }

    /// Marks every dirty node, whose block `blk` now holds, clean.
    fn clean(&mut self) {
        for s in self.base..self.top {
            if self.dirt[s] {
                (self.dirt[s], self.stamp[s]) = (false, self.clock);
            }
        }
        self.ndirty = 0;
    }

    /// The first free block at or after `b`, marked live; `b` moves past it.
    fn claim(&mut self, b: &mut u64) -> u64 {
        *b = self.next_free(*b);
        self.mark(*b);
        *b += 1;
        *b - 1
    }

    /// Seals the dirty nodes bottom up, filling each parent's pointers with its children's blocks and sums.
    fn finalize(&mut self, generation: u64) {
        for level in 0..self.height {
            for s in self.base..self.top {
                if !self.dirt[s] || self.cache[s][0] as usize != level {
                    continue;
                }
                for i in 0..if level > 0 { count(&self.cache[s]) } else { 0 } {
                    let b = eptr(&self.cache[s], i).block;
                    if b & TAG != 0 {
                        let c = (b & !TAG) as usize;
                        let p = Ptr {
                            block: self.blk[c],
                            sum: le64(&self.cache[c], END),
                            generation,
                        };
                        set_eptr(&mut self.cache[s], i, p);
                    }
                }
                seal(self.blk[s], &mut self.cache[s], END);
            }
        }
        if self.root.block & TAG != 0 {
            let s = (self.root.block & !TAG) as usize;
            self.root = Ptr {
                block: self.blk[s],
                sum: le64(&self.cache[s], END),
                generation,
            };
        }
    }

    /// Writes staging slots `first..n`, then every dirty node copied after them through the staging slots, in as
    /// few requests as their blocks and the staging room allow.
    fn write_out(&mut self, mut first: usize, mut n: usize) -> Result<(), Error> {
        for s in self.base..self.top {
            if !self.dirt[s] {
                continue;
            }
            if n == self.base {
                self.write_slots(first, n)?;
                (first, n) = (1, 1);
            }
            let (src, dst) = pair(self.cache, s, n);
            dst.copy_from_slice(src);
            self.blk[n] = self.blk[s];
            n += 1;
        }
        self.write_slots(first, n)
    }

    /// Writes slots `first..end` to their blocks, one request per run of consecutive blocks.
    fn write_slots(&mut self, first: usize, end: usize) -> Result<(), Error> {
        let mut a = first;
        while a < end {
            let mut b = a + 1;
            while b < end && self.blk[b] == self.blk[b - 1] + 1 {
                b += 1;
            }
            let r = self.disk.write(self.blk[a], &self.cache[a..b]);
            self.broken |= r.is_err();
            r?;
            a = b;
        }
        Ok(())
    }
}

fn tagged(s: usize) -> Ptr {
    Ptr {
        block: TAG | s as u64,
        sum: 0,
        generation: 0,
    }
}

fn key(inode: u64, kind: u64, offset: u64) -> u128 {
    ((inode as u128) << 64) + ((kind as u128) << 62) + offset as u128
}

fn kind_of(kind: u8) -> Kind {
    if kind == DIR { Kind::Dir } else { Kind::File }
}

fn count_set(set: &[u64; 4]) -> usize {
    set.iter().map(|w| w.count_ones() as usize).sum()
}

fn bit(set: &[u64; 4], i: usize) -> bool {
    set[i / 64] >> (i % 64) & 1 != 0
}

/// Two distinct slots of `cache`, mutably.
fn pair(cache: &mut [Block], a: usize, b: usize) -> (&mut Block, &mut Block) {
    if a < b {
        let (x, y) = cache.split_at_mut(b);
        (&mut x[a], &mut y[0])
    } else {
        let (x, y) = cache.split_at_mut(a);
        (&mut y[0], &mut x[b])
    }
}

/// Slot `slot`'s superblock if its sum, magic and generation hold; `valid` if every other field does too.
fn superblock(sb: &Block, slot: u64, disk: u64) -> Option<Super> {
    let f = |i: usize| le64(sb, 8 * i);
    if le64(sb, END) != checksum(slot, &sb[..SB_LEN]) || f(0) != MAGIC || f(1) % 2 != slot {
        return None;
    }
    let mut s = Super {
        flags: f(3),
        valid: false,
        generation: f(1),
        blocks: f(2),
        next_inode: f(4),
        seed: f(5),
        root: Ptr {
            block: f(9),
            sum: f(10),
            generation: f(11),
        },
        level: f(12) as usize,
        index: (f(13), f(14)),
    };
    s.valid = (MIN_BLOCKS..=disk).contains(&s.blocks)
        && s.next_inode >= 1
        && f(6) == 0
        && f(7) == 0
        && f(8) == 1
        && f(12) < MAX_HEIGHT as u64
        && s.root.generation <= s.generation
        && (2..s.blocks).contains(&s.root.block)
        && (2..s.blocks).contains(&s.index.0)
        && sb[SB_LEN..END].iter().all(|&b| b == 0);
    Some(s)
}

fn encode(v: &mut [u8], it: &Item) {
    v[0] = it.kind;
    v[1] = 0;
    v[2..4].copy_from_slice(&it.mode.to_le_bytes());
    v[4..8].copy_from_slice(&it.links.to_le_bytes());
    for (i, f) in [it.size, it.parent, it.entry, it.mtime, it.ctime, it.btime]
        .iter()
        .enumerate()
    {
        v[8 + 8 * i..16 + 8 * i].copy_from_slice(&f.to_le_bytes());
    }
}

fn decode(v: &[u8]) -> Item {
    Item {
        kind: v[0],
        mode: le16(v, 2) as u16,
        links: u32::from_le_bytes(v[4..8].try_into().unwrap()),
        size: le64(v, 8),
        parent: le64(v, 16),
        entry: le64(v, 24),
        mtime: le64(v, 32),
        ctime: le64(v, 40),
        btime: le64(v, 48),
    }
}

fn count(n: &[u8]) -> usize {
    le16(n, 2)
}

fn set_count(n: &mut [u8], c: usize) {
    n[2..4].copy_from_slice(&(c as u16).to_le_bytes());
}

fn ikey(n: &[u8], i: usize) -> u128 {
    le128(n, HDR + ITEM * i)
}

fn voff(n: &[u8], i: usize) -> usize {
    le16(n, HDR + ITEM * i + 16)
}

fn vlen(n: &[u8], i: usize) -> usize {
    le16(n, HDR + ITEM * i + 18)
}

fn value(n: &[u8], i: usize) -> &[u8] {
    &n[voff(n, i)..voff(n, i) + vlen(n, i)]
}

fn ekey(n: &[u8], i: usize) -> u128 {
    le128(n, HDR + ENTRY * i)
}

fn eptr(n: &[u8], i: usize) -> Ptr {
    let at = HDR + ENTRY * i;
    Ptr {
        block: le64(n, at + 16),
        sum: le64(n, at + 24),
        generation: le64(n, at + 32),
    }
}

fn set_eptr(n: &mut [u8], i: usize, p: Ptr) {
    let at = HDR + ENTRY * i + 16;
    for (j, f) in [p.block, p.sum, p.generation].iter().enumerate() {
        n[at + 8 * j..at + 8 * j + 8].copy_from_slice(&f.to_le_bytes());
    }
}

fn set_eblock(n: &mut [u8], i: usize, block: u64) {
    n[HDR + ENTRY * i + 16..][..8].copy_from_slice(&block.to_le_bytes());
}

/// Bytes a node's items or entries take (with their values).
fn used(n: &[u8]) -> usize {
    let c = count(n);
    match (n[0], c) {
        (0, 0) => 0,
        (0, _) => ITEM * c + END - voff(n, c - 1),
        _ => ENTRY * c,
    }
}

/// The first item at or after `k`.
fn search(n: &[u8], k: u128) -> usize {
    let (mut a, mut b) = (0, count(n));
    while a < b {
        let m = (a + b) / 2;
        if ikey(n, m) < k {
            a = m + 1;
        } else {
            b = m;
        }
    }
    a
}

/// The child whose range holds `k`.
fn route(n: &[u8], k: u128) -> usize {
    let (mut a, mut b) = (1, count(n));
    while a < b {
        let m = (a + b) / 2;
        if ekey(n, m) <= k {
            a = m + 1;
        } else {
            b = m;
        }
    }
    a - 1
}

/// Opens a `len`-byte value for item `k` at index `i`; returns its offset. The leaf must have room.
fn leaf_insert(n: &mut [u8], i: usize, k: u128, len: usize) -> usize {
    let c = count(n);
    let top = if i == 0 { END } else { voff(n, i - 1) };
    let bottom = if c == 0 { END } else { voff(n, c - 1) };
    n.copy_within(bottom..top, bottom - len);
    n.copy_within(HDR + ITEM * i..HDR + ITEM * c, HDR + ITEM * (i + 1));
    for j in i + 1..=c {
        let at = HDR + ITEM * j + 16;
        let off = voff(n, j) - len;
        n[at..at + 2].copy_from_slice(&(off as u16).to_le_bytes());
    }
    let at = HDR + ITEM * i;
    n[at..at + 16].copy_from_slice(&k.to_le_bytes());
    n[at + 16..at + 18].copy_from_slice(&((top - len) as u16).to_le_bytes());
    n[at + 18..at + 20].copy_from_slice(&(len as u16).to_le_bytes());
    set_count(n, c + 1);
    top - len
}

fn leaf_remove(n: &mut [u8], i: usize) {
    let c = count(n);
    let (off, len, bottom) = (voff(n, i), vlen(n, i), voff(n, c - 1));
    n.copy_within(bottom..off, bottom + len);
    n.copy_within(HDR + ITEM * (i + 1)..HDR + ITEM * c, HDR + ITEM * i);
    for j in i..c - 1 {
        let at = HDR + ITEM * j + 16;
        let off = voff(n, j) + len;
        n[at..at + 2].copy_from_slice(&(off as u16).to_le_bytes());
    }
    set_count(n, c - 1);
}

/// Appends `src`'s items from `from` on to `dst`, which must have room, and drops them from `src`.
fn leaf_move(src: &mut [u8], from: usize, dst: &mut [u8]) {
    for i in from..count(src) {
        let len = vlen(src, i);
        let at = leaf_insert(dst, count(dst), ikey(src, i), len);
        dst[at..at + len].copy_from_slice(value(src, i));
    }
    set_count(src, from);
}

fn internal_insert(n: &mut [u8], i: usize, k: u128, p: Ptr) {
    let c = count(n);
    n.copy_within(HDR + ENTRY * i..HDR + ENTRY * c, HDR + ENTRY * (i + 1));
    n[HDR + ENTRY * i..][..16].copy_from_slice(&k.to_le_bytes());
    set_eptr(n, i, p);
    set_count(n, c + 1);
}

fn internal_remove(n: &mut [u8], i: usize) {
    let c = count(n);
    n.copy_within(HDR + ENTRY * (i + 1)..HDR + ENTRY * c, HDR + ENTRY * i);
    set_count(n, c - 1);
}

fn internal_move(src: &mut [u8], from: usize, dst: &mut [u8]) {
    let (c, d) = (count(src), count(dst));
    dst[HDR + ENTRY * d..][..ENTRY * (c - from)]
        .copy_from_slice(&src[HDR + ENTRY * from..HDR + ENTRY * c]);
    set_count(dst, d + c - from);
    set_count(src, from);
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != b"."
        && name != b".."
        && !name.contains(&b'/')
        && !name.contains(&0)
}

fn mix(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29)
}

/// The first offset of `name`'s hash chain: a seeded multiply-rotate hash with a final mix.
fn name_hash(seed: u64, name: &[u8]) -> u64 {
    let (words, rest) = name.as_chunks::<8>();
    let mut h = words.iter().fold(seed ^ name.len() as u64, |h, w| {
        mix(h, u64::from_le_bytes(*w))
    });
    if !rest.is_empty() {
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        h = mix(h, u64::from_le_bytes(last));
    }
    h ^= h >> 31;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 29;
    (h >> 5) << 3
}

/// Sixteen interleaved multiply-rotate lanes over 64-bit words; each step is a bijection, so any one-word change shows.
fn checksum(block: u64, buf: &[u8]) -> u64 {
    let (words, _) = buf.as_chunks::<8>();
    let mut lanes: [u64; 16] = core::array::from_fn(|i| i as u64);
    lanes[0] ^= block | 1 << 63;
    for chunk in words.chunks(16) {
        for (l, w) in lanes.iter_mut().zip(chunk) {
            *l = mix(*l, u64::from_le_bytes(*w));
        }
    }
    lanes.into_iter().fold(0, mix)
}

/// Writes the sum of `block` and `n`'s first `len` bytes into its last 8; returns it.
fn seal(block: u64, n: &mut Block, len: usize) -> u64 {
    let sum = checksum(block, &n[..len]);
    n[END..].copy_from_slice(&sum.to_le_bytes());
    sum
}

fn le16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn le128(b: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(b[at..at + 16].try_into().unwrap())
}

#[cfg(test)]
mod tests;
