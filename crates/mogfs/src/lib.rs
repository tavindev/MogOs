//! MogFS: a checksummed copy-on-write B+tree file system over 4 KiB blocks.
//!
//! Format (little-endian):
//! - Blocks 0 and 1 are superblock slots; generation `g` goes to slot `g % 2`, and mount takes the valid slot with
//!   the highest generation. Superblock: magic u64, generation u64, block count u64, incompatible features u64 (none
//!   are known yet; a set bit refuses the mount), next inode u64, name-hash seed u64, pinned bitmap index (block u64,
//!   sum u64; zero until snapshots), root count u32 (1: the live root), 4 zero bytes, the live root (tree root block
//!   u64, sum u64, birth generation u64, level u64), log length u64, bitmap index height u64, then the live bitmap's
//!   page list: for height 0 (at most 128 pages) a (block u64, sum u64) for each 128 MiB page of the disk (block 0:
//!   an all-zero page), else the (block, sum) of the index root; then the log, (word u64, value u64) in increasing
//!   word order, and zeros. An index block at level 1 lists up to 255 pages' (block, sum), one at level `l` up to 255
//!   level `l - 1` blocks', zeros after them (block 0: all of its pages are zero); the root is the one block at the
//!   top level, the height the fewest levels that list every page.
//! - Superblocks, tree nodes and index blocks end in a 64-bit hash of their block number and their bytes (an index
//!   block's entries), and every pointer to one holds that hash (a node pointer also its birth generation), so a lost
//!   or misdirected write of a valid old block reads as corrupt.
//! - Data and bitmap pages are whole 4096-byte blocks; the hash of each (block number and page) lives in the extent
//!   or page list pointing at it.
//! - Free space is stored: bit `b % 64` of u64 word `b / 64` is set if the root reaches block `b` (the superblocks,
//!   its tree nodes and data, its bitmap pages and index blocks); a word is its log entry's value if it has one,
//!   else its page's.
//! - Tree: one B+tree keyed by u128 `inode << 64 | kind << 62 | offset`. Node header: level u8 (0 = leaf), zero u8,
//!   count u16, then for a leaf the offset of its lowest value u16, else zero, and 2 zero bytes. Leaf: `count` items
//!   (key u128, value offset u16, value length u16); the values fill the bytes from the lowest up to byte 4088, in any
//!   order, without overlap. Internal: `count` entries (key u128, block u64, sum u64, birth generation u64);
//!   child `i` holds keys from entry `i`'s (the node's own lower bound for `i = 0`) up to entry `i + 1`'s.
//! - Items. Inode (kind 0, offset 0): kind u8 (1 file, 2 directory, 3 symlink: reserved), zero u8, mode u16, links
//!   u32, size u64, parent u64, entry u64 (the offset of the entry naming it in `parent`), mtime, ctime and btime u64
//!   (ns). Directory entry (kind 1; offset: the seeded hash of the name, low 3 bits the slot in its collision chain of
//!   8): inode u64 (never the root or the directory itself), kind u8, name; it is followed only if that inode records
//!   it back (parent, entry and kind). Extent (kind 2; offset: its first page): first block u64, then each of its 1
//!   to 128 pages' sums.
//! - Inode numbers come from the superblock's counter and are never reused.
//! - Copy-on-write: no block reachable from either slot is written. Data pages are written at once to free blocks
//!   (or over one written since the last commit). `commit` gives the dirty nodes consecutive free blocks, writes them
//!   in one request, flushes, writes the other slot with every word changed since the pages were written in its log,
//!   and flushes; when the log would overflow, the changed pages and the index blocks above them join the nodes'
//!   request and the log empties.
//!   Contract: the committed state is always a consistent snapshot of the file system as of a `commit` call.
#![cfg_attr(not(test), no_std)]

use core::cmp::min;
use core::ops::{Add, Sub};
use core::slice::{from_mut, from_ref};

pub const BLOCK_SIZE: usize = 4096;
/// The contents of one block.
pub type Buf = [u8; BLOCK_SIZE];
pub const NAME_MAX: usize = 255;
pub const MAX_FILE_SIZE: u64 = 1 << 52;
/// Pages an extent maps at most.
pub const EXTENT_MAX: u64 = 128;
/// Largest file system, in blocks: block numbers keep their top bit clear. `format` uses at most this much of a disk.
pub const MAX_BLOCKS: u64 = 1 << 62;
/// Fewest node cache slots `Fs` accepts.
pub const MIN_POOL: usize = 32;
/// The root directory.
pub const ROOT: Inode = Inode(0);

// Unit tests shrink pages, index blocks and the inline list, so small disks reach several index levels.
#[cfg(not(test))]
const PAGE_BITS: u64 = 8 * BLOCK_SIZE as u64;
#[cfg(test)]
const PAGE_BITS: u64 = 512;
const PAGE_WORDS: usize = (PAGE_BITS / 64) as usize;
/// Entries an index block lists.
#[cfg(not(test))]
const FAN: usize = (BLOCK_SIZE - 8) / 16;
#[cfg(test)]
const FAN: usize = 4;
/// Pages the superblock lists itself; more take an index.
#[cfg(not(test))]
const INLINE: usize = 128;
#[cfg(test)]
const INLINE: usize = 2;
/// Index levels `mount` accepts; past `MAX_BLOCKS` with real sizes.
const MAX_IX: usize = 24;
const MAX_POOL: usize = 512;
/// Cache slots that stage a commit's request (the last also the mount's pre-log words), after the scratch slot.
const STAGE: usize = 32;
const MAX_CACHE: usize = 1 + STAGE + MAX_POOL;
/// Superblock bytes before the page list: the header and a root table of one root.
const SB_HDR: usize = 120;
/// Log entries the superblock has room for beside one page.
const LOG_MAX: usize = (END - SB_HDR) / 16 - 1;
const MIN_BLOCKS: u64 = 16;
const MAX_HEIGHT: usize = 8;
const MAGIC: u64 = u64::from_le_bytes(*b"MogFS\0\0\x02");
const CHAIN: u64 = 8;
/// Lookups `lookup` remembers.
const NAMES: usize = 4;

const END: usize = BLOCK_SIZE - 8;
const HDR: usize = 8;
const ITEM: usize = 20;
const ENTRY: usize = 40;
const CAP: usize = END - HDR;
const FANOUT: usize = CAP / ENTRY;
const QUARTER: usize = CAP / 4;
const INODE_LEN: usize = 56;
const EXTENT_ITEM: usize = ITEM + 8 + 8 * EXTENT_MAX as usize;

const OFFSET: u64 = (1 << 62) - 1;
const FILE: u8 = 1;
const DIR: u8 = 2;

/// A pointer's block with this bit set names the dirty node in that cache slot.
const TAG: u64 = 1 << 63;
const EMPTY: Block = Block(u64::MAX);
const NONE: Key = Key(u128::MAX);

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
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error>;
    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error>;
    /// Returns once every completed write is durable.
    fn flush(&mut self) -> Result<(), Error>;
    fn blocks(&self) -> u64;
}

/// A file or directory; its number is never reused.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode(u64);

/// A block's number on the disk.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Block(pub u64);

/// A page of a file: its bytes from `BLOCK_SIZE` times this number on.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Page(pub u64);

/// A block's checksum; `map` gives a data page's for `verify`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sum(u64);

impl Add<u64> for Block {
    type Output = Block;
    #[inline(always)]
    fn add(self, n: u64) -> Block {
        Block(self.0 + n)
    }
}

impl Sub for Block {
    type Output = u64;
    #[inline(always)]
    fn sub(self, b: Block) -> u64 {
        self.0 - b.0
    }
}

impl Add<u64> for Page {
    type Output = Page;
    #[inline(always)]
    fn add(self, n: u64) -> Page {
        Page(self.0 + n)
    }
}

impl Sub for Page {
    type Output = u64;
    #[inline(always)]
    fn sub(self, p: Page) -> u64 {
        self.0 - p.0
    }
}

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
    1 + STAGE + pool + ix_blocks(pages(blocks))
}

/// Bitmap words `Fs` needs for a disk of `blocks` blocks.
pub const fn bitmap_words(blocks: u64) -> usize {
    let pages = pages(blocks);
    3 * pages * PAGE_WORDS + pages.div_ceil(64)
}

/// Whether `page` read from `block` matches the sum `map` gave for it.
pub fn verify(block: Block, page: &Buf, sum: Sum) -> Result<(), Error> {
    if checksum(block, page) == sum {
        Ok(())
    } else {
        Err(Error::Corrupt)
    }
}

const fn pages(blocks: u64) -> usize {
    blocks.div_ceil(PAGE_BITS) as usize
}

/// Levels of the bitmap index for `pages` pages (0: the superblock lists them).
const fn ix_height(pages: usize) -> usize {
    let (mut n, mut h) = (pages, 0);
    if pages <= INLINE {
        return 0;
    }
    while n > 1 {
        n = n.div_ceil(FAN);
        h += 1;
    }
    h
}

/// Cache slots the live page list takes: its index blocks, or one for an inline list.
const fn ix_blocks(pages: usize) -> usize {
    let (mut n, mut t) = (pages, 0);
    if pages <= INLINE {
        return 1;
    }
    while n > 1 {
        n = n.div_ceil(FAN);
        t += n;
    }
    t
}

/// Blocks at index level `l` (0: the pages themselves) of `pages` pages.
fn ix_count(pages: usize, l: usize) -> usize {
    (0..l).fold(pages, |n, _| n.div_ceil(FAN))
}

/// Entries index block `i` at level `l` holds.
fn ix_children(pages: usize, l: usize, i: usize) -> usize {
    min(FAN, ix_count(pages, l - 1) - FAN * i)
}

/// An item key's low 62 bits: 0 for an inode, a name's chain slot for an entry, the first page for an extent.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Offset(u64);

impl Add<u64> for Offset {
    type Output = Offset;
    #[inline(always)]
    fn add(self, n: u64) -> Offset {
        Offset(self.0 + n)
    }
}

impl Sub for Offset {
    type Output = u64;
    #[inline(always)]
    fn sub(self, o: Offset) -> u64 {
        self.0 - o.0
    }
}

enum ItemKind {
    Inode = 0,
    Entry = 1,
    Extent = 2,
}

/// A tree key: `inode << 64 | kind << 62 | offset`.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Key(u128);

impl Key {
    #[inline(always)]
    fn new(inode: Inode, kind: ItemKind, offset: Offset) -> Key {
        debug_assert!(offset.0 <= OFFSET);
        Key(((inode.0 as u128) << 64) + ((kind as u128) << 62) + offset.0 as u128)
    }

    #[inline(always)]
    fn inode(self) -> Inode {
        Inode((self.0 >> 64) as u64)
    }

    #[inline(always)]
    fn kind(self) -> Option<ItemKind> {
        match (self.0 as u64) >> 62 {
            0 => Some(ItemKind::Inode),
            1 => Some(ItemKind::Entry),
            2 => Some(ItemKind::Extent),
            _ => None,
        }
    }

    #[inline(always)]
    fn offset(self) -> Offset {
        Offset(self.0 as u64 & OFFSET)
    }
}

/// The key of `inode`'s extent starting at `page`.
#[inline(always)]
fn extent_key(inode: Inode, page: Page) -> Key {
    Key::new(inode, ItemKind::Extent, Offset(page.0))
}

#[derive(Clone, Copy)]
struct Ptr {
    block: Block,
    sum: Sum,
    generation: u64,
}

impl Ptr {
    /// The cache slot of the dirty node it names, if it names one.
    #[inline(always)]
    fn slot(self) -> Option<usize> {
        (self.block.0 & TAG != 0).then_some((self.block.0 & !TAG) as usize)
    }
}

#[derive(Clone, Copy)]
struct Item {
    kind: u8,
    mode: u16,
    links: u32,
    size: u64,
    parent: Inode,
    /// The offset of the one directory entry that names it.
    entry: Offset,
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
    /// Log entries after the page list.
    log: usize,
    /// Bitmap index height, and its root if it has levels.
    ix_h: usize,
    ix: (Block, Sum),
}

/// A directory entry's offset, inode and kind.
type Entry = (Offset, Inode, u8);

/// The dirty nodes from the root down to a leaf: slot, child index taken, and key bounds at each level.
#[derive(Default)]
struct Path {
    slot: [usize; MAX_HEIGHT],
    idx: [usize; MAX_HEIGHT],
    lo: [Key; MAX_HEIGHT],
    hi: [Key; MAX_HEIGHT],
}

/// A file system on `D` with memory its caller gives: `cache` (staging, node slots and the page list, `cache_blocks`) and
/// `bits` (`bitmap_words`). `Io` from any change, or a failed `mount`, leaves it refusing writes and commits until a
/// `mount` succeeds; after `Io` from `commit`, durability is unknown.
pub struct Fs<'a, D> {
    disk: D,
    cache: &'a mut [Buf],
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
    /// Bitmap index height, and its root's block and sum (zero for an inline list).
    ix_h: usize,
    ix: (Block, Sum),
    /// The live bitmap's words changed since its pages were last written, sorted, unless `full`: the next commit
    /// writes the pages. `bits` marks the pages changed since they were written, after the three bitmaps.
    log: [u64; LOG_MAX],
    nlog: usize,
    full: bool,
    /// The live bitmap's words changed since the last commit (`lo..hi`), and those the last commit changed.
    span: (usize, usize),
    prev_span: (usize, usize),
    changed: bool,
    free: u64,
    /// Every block below it is in use.
    hint: Block,
    /// `cache[0]` is scratch, `cache[1..base]` stage commits (the last also the mount's pre-log words),
    /// `cache[base..top]` cache nodes, and `cache[top..]` the live page list: its index blocks, level 1 first, or one
    /// slot for an inline list.
    base: usize,
    top: usize,
    /// Each cache slot's block (`EMPTY` if none or not yet given), whether it holds a dirty node, and last use.
    blk: [Block; MAX_CACHE],
    dirt: [bool; MAX_CACHE],
    ndirty: usize,
    stamp: [u64; MAX_CACHE],
    clock: u64,
    /// `DATA` (a data page) and `META` (superblocks, and scratch for an extent's value).
    bufs: [Buf; 2],
    /// The block and sum `bufs[DATA]` holds, and the file page it is; `unwritten` if its block is not written yet
    /// (a write goes to the disk only when the buffer is needed for another page, at `commit` or at `map`).
    cached: Option<(Block, Sum, Inode, Page)>,
    unwritten: bool,
    /// The last two inode items read, newest first; any change to an inode item clears them.
    items: [Option<(Inode, Item)>; 2],
    /// The last lookups that found their entry: directory, name length (0: none) and bytes, and the inode; any change
    /// to an entry clears them.
    names: [(Inode, u8, [u8; NAME_MAX], Inode); NAMES],
    next_name: usize,
    /// The leaf the last descent reached, with its key bounds; cleared before a slot is reused or the tree's shape
    /// changes. `readdir` keeps there the last key it searched for and its item index.
    finger: Option<(usize, Key, Key)>,
    start: (Key, usize),
    broken: bool,
}

impl<'a, D: Disk> Fs<'a, D> {
    /// An unmounted file system; `mount` or `format` it before use.
    pub const fn new(disk: D, cache: &'a mut [Buf], bits: &'a mut [u64]) -> Self {
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
                block: Block(0),
                sum: Sum(0),
                generation: 0,
            },
            height: 1,
            ix_h: 0,
            ix: (Block(0), Sum(0)),
            log: [0; LOG_MAX],
            nlog: 0,
            full: false,
            span: (usize::MAX, 0),
            prev_span: (usize::MAX, 0),
            changed: false,
            free: 0,
            hint: Block(0),
            base: 0,
            top: 0,
            blk: [EMPTY; MAX_CACHE],
            dirt: [false; MAX_CACHE],
            ndirty: 0,
            stamp: [0; MAX_CACHE],
            clock: 0,
            bufs: [[0; BLOCK_SIZE]; 2],
            cached: None,
            unwritten: false,
            items: [None; 2],
            names: [(ROOT, 0, [0; NAME_MAX], ROOT); NAMES],
            next_name: 0,
            finger: None,
            start: (NONE, 0),
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
        let top = self.top;
        for b in &mut self.cache[top..top + ix_blocks(self.pages)] {
            b.fill(0);
        }
        self.ix = (Block(0), Sum(0));
        (self.generation, self.next_inode, self.seed) = (0, 1, seed);
        let s = self.new_node(0);
        self.root = tagged(s);
        self.height = 1;
        let root = Item {
            kind: DIR,
            mode: 0o755,
            links: 1,
            size: 0,
            parent: ROOT,
            entry: Offset(0),
            mtime: self.now,
            ctime: self.now,
            btime: self.now,
        };
        let (s, at) = self.insert(Key::new(ROOT, ItemKind::Inode, Offset(0)), INODE_LEN)?;
        encode(&mut self.cache[s][at..at + INODE_LEN], &root);
        self.full = true;
        self.commit()
    }

    /// Loads the newest valid slot, or the older one if the newest's bitmap or rightmost path is corrupt; drops
    /// uncommitted changes. Blocks the other slot's bitmap marks stay reserved.
    pub fn mount(&mut self) -> Result<(), Error> {
        self.broken = true;
        (self.cached, self.unwritten, self.items) = (None, false, [None; 2]);
        (self.finger, self.start) = (None, (NONE, 0));
        self.forget_names();
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
            self.bits[COMMITTED * w..3 * w].fill(0);
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
            let d = 3 * w;
            self.bits[d..d + self.pages.div_ceil(64)].fill(0);
            for j in 0..self.nlog {
                let p = self.log[j] as usize / PAGE_WORDS;
                self.bits[d + p / 64] |= 1 << (p % 64);
            }
            self.prev_span = (0, w);
            self.changed = false;
            let used: u64 = (0..w)
                .map(|i| (self.bits[i] | self.bits[COMMITTED * w + i]).count_ones() as u64)
                .sum();
            self.free = self.blocks - used;
            self.hint = Block(0);
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
        for j in 0..NAMES {
            let (d, len, n, i) = &self.names[j];
            if *d == dir && *len as usize == name.len() && n[..name.len()] == *name {
                return Ok(*i);
            }
        }
        let e = self.find_entry(dir, name)?.0.ok_or(Error::NotFound)?;
        self.child(dir, e)?;
        let (d, len, n, i) = &mut self.names[self.next_name];
        (*d, *len, *i) = (dir, name.len() as u8, e.1);
        n[..name.len()].copy_from_slice(name);
        self.next_name = (self.next_name + 1) % NAMES;
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
        let mut k = Key::new(dir, ItemKind::Entry, Offset(cursor));
        let end = Key::new(dir, ItemKind::Extent, Offset(0));
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            let first = match self.start {
                (key, i) if key == k => i,
                _ => search(n, k),
            };
            self.start = (k, first);
            for i in first..count(n) {
                let k = ikey(n, i);
                if k >= end {
                    return Ok(u64::MAX);
                }
                let v = value(n, i);
                if f(&v[9..], Inode(le64(v, 0)), kind_of(v[8])) {
                    return Ok(k.offset().0);
                }
            }
            if hi >= end {
                return Ok(u64::MAX);
            }
            k = hi;
        }
    }

    pub fn kind(&mut self, inode: Inode) -> Result<Kind, Error> {
        Ok(kind_of(self.inode(inode)?.kind))
    }

    pub fn stat(&mut self, inode: Inode) -> Result<Stat, Error> {
        let it = *self.inode(inode)?;
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
        let end = min(
            self.file(file)?.size,
            offset.saturating_add(buf.len() as u64),
        );
        let mut pos = offset;
        while pos < end {
            let (page, at) = (
                Page(pos / BLOCK_SIZE as u64),
                (pos % BLOCK_SIZE as u64) as usize,
            );
            let n = min(BLOCK_SIZE - at, (end - pos) as usize);
            let out = &mut buf[(pos - offset) as usize..][..n];
            pos += n as u64;
            if !self.cached.is_some_and(|(.., i, p)| i == file && p == page) {
                let Some((off, start, _)) = self.extent_at(file, page)? else {
                    out.fill(0);
                    continue;
                };
                let j = (page - off) % EXTENT_MAX;
                let sum = Sum(le64(&self.bufs[META], 8 + 8 * j as usize));
                self.load_page(start + j, sum, file, page)?;
            }
            out.copy_from_slice(&self.bufs[DATA][at..at + n]);
        }
        Ok(end.saturating_sub(offset) as usize)
    }

    /// The block holding `page` of `file` and that page's sum (check it with `verify`), or `None` for a hole.
    pub fn map(&mut self, file: Inode, page: Page) -> Result<Option<(Block, Sum)>, Error> {
        self.file(file)?;
        if self.cached.is_some_and(|(.., i, p)| i == file && p == page) {
            self.write_data()?;
        }
        if page.0 >= MAX_FILE_SIZE / BLOCK_SIZE as u64 {
            return Ok(None);
        }
        Ok(self.extent_at(file, page)?.map(|(off, start, _)| {
            let j = (page - off) % EXTENT_MAX;
            (start + j, Sum(le64(&self.bufs[META], 8 + 8 * j as usize)))
        }))
    }

    /// Writes `data` at `offset`, growing the file; a gap past the old end reads as zeros. `NoSpace` changes nothing.
    pub fn write(&mut self, file: Inode, offset: u64, data: &[u8]) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let mut it = *self.file(file)?;
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
        self.reserve(pages, 3, bytes, false)?;
        let r = self.write_pages(file, offset, data).and_then(|()| {
            it.size = it.size.max(end);
            (it.mtime, it.ctime) = (self.now, self.now);
            self.set_inode(file, &it)
        });
        self.broken |= r.is_err();
        r
    }

    /// Empties `file`.
    pub fn truncate(&mut self, file: Inode) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let it = self.file(file)?;
        // A file's extents end within its size (only a truncate shrinks it), so an empty one has none.
        if it.size == 0 {
            return Ok(());
        }
        let mut it = *it;
        let bytes = self.extent_bytes(file)?;
        self.reserve(0, 2, bytes, true)?;
        let r = self.remove_extents(file).and_then(|()| {
            it.size = 0;
            (it.mtime, it.ctime) = (self.now, self.now);
            self.set_inode(file, &it)
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
        let mut d = *self.dir(dir)?;
        let e = self.find_entry(dir, name)?.0.ok_or(Error::NotFound)?;
        let (off, inode, _) = e;
        let it = *self.child(dir, e)?;
        if it.kind == DIR
            && let Some((s, i, _)) = self.seek(Key::new(inode, ItemKind::Entry, Offset(0)))?
            && ikey(&self.cache[s], i) < Key::new(inode, ItemKind::Extent, Offset(0))
        {
            return Err(Error::NotEmpty);
        }
        // As in `truncate`: an empty file or a directory has no extents.
        let bytes = if it.size > 0 {
            self.extent_bytes(inode)?
        } else {
            0
        };
        self.reserve(0, 4, bytes, true)?;
        self.forget_names();
        let r = self
            .delete(Key::new(dir, ItemKind::Entry, off))
            .and_then(|()| {
                if it.size > 0 {
                    self.remove_extents(inode)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                self.items = [None; 2];
                self.delete(Key::new(inode, ItemKind::Inode, Offset(0)))
            })
            .and_then(|()| {
                (d.mtime, d.ctime) = (self.now, self.now);
                self.set_inode(dir, &d)
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
        let mut from = *self.dir(from_dir)?;
        let e = self
            .find_entry(from_dir, from_name)?
            .0
            .ok_or(Error::NotFound)?;
        let (off, inode, kind) = e;
        let mut it = *self.child(from_dir, e)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        let mut to = *self.dir(to_dir)?;
        let (taken, slot) = self.find_entry(to_dir, to_name)?;
        if taken.is_some() {
            return Err(Error::Exists);
        }
        let slot = slot.ok_or(Error::Collision)?;
        if from_dir != to_dir && kind == DIR && self.below(inode, to_dir)? {
            return Err(Error::InvalidName);
        }
        self.reserve(0, 5, 0, false)?;
        self.forget_names();
        let r = self
            .delete(Key::new(from_dir, ItemKind::Entry, off))
            .and_then(|()| self.put_entry(to_dir, slot, inode, kind, to_name))
            .and_then(|()| {
                (it.ctime, it.parent, it.entry) = (self.now, to_dir, slot);
                self.set_inode(inode, &it)?;
                (from.mtime, from.ctime) = (self.now, self.now);
                self.set_inode(from_dir, &from)?;
                if to_dir != from_dir {
                    (to.mtime, to.ctime) = (self.now, self.now);
                    self.set_inode(to_dir, &to)?;
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
        // Each node claimed adds at most one word to the log.
        let pages = self.full || self.nlog + self.ndirty > self.log_cap();
        let start = if pages {
            // Release the old copies of the pages that change and of the index blocks above them (each pointer zeroed
            // once released), then find blocks for them and the nodes; claiming those blocks may change more pages,
            // so repeat until it does not.
            loop {
                let mut k = self.ndirty;
                let mut p = self.next_dirty(0);
                while let Some(q) = p {
                    let (s, at) = self.page_entry(q);
                    self.release_entry(s, at)?;
                    k += 1;
                    p = self.next_dirty(q + 1);
                }
                for l in 1..=self.ix_h {
                    let mut i = self.next_ix(l, 0);
                    while let Some(j) = i {
                        match self.ix_parent(l, j) {
                            Some((s, at)) => self.release_entry(s, at)?,
                            None if self.ix.0 != Block(0) => {
                                self.release(self.ix.0)?;
                                self.ix = (Block(0), Sum(0));
                            }
                            None => {}
                        }
                        k += 1;
                        i = self.next_ix(l, j + 1);
                    }
                }
                if (self.free as usize) < k {
                    return Err(Error::NoSpace);
                }
                let start = self.start(k);
                let (mut b, mut more) = (start, false);
                for _ in 0..k {
                    b = self.next_free(b);
                    let p = (b.0 / PAGE_BITS) as usize;
                    more |= !self.page_dirty(p);
                    self.bits[3 * self.words + p / 64] |= 1 << (p % 64);
                    b = b + 1;
                }
                if !more {
                    break start;
                }
            }
        } else {
            if (self.free as usize) < self.ndirty {
                return Err(Error::NoSpace);
            }
            // An unwritten data page leads the nodes' request: where it is if the blocks after it are free, else
            // moved to the start of their run when the block after the run is free too.
            match self.cached {
                Some((d, ..)) if self.unwritten && self.run_free(d + 1, self.ndirty) => d + 1,
                Some(_) if self.unwritten => {
                    let b = self.start(self.ndirty);
                    if self.run_free(b, self.ndirty + 1) && self.lead_data(b)? {
                        b + 1
                    } else {
                        b
                    }
                }
                _ => self.start(self.ndirty),
            }
        };
        let (mut first, mut n) = (1, 1);
        match self.cached {
            Some((d, ..)) if self.unwritten && !pages && start == d + 1 => {
                let s = self.stage(&mut first, &mut n)?;
                let (src, dst) = (&self.bufs[DATA], &mut self.cache[s]);
                dst.copy_from_slice(src);
                (self.blk[s], self.unwritten) = (d, false);
            }
            _ => self.write_data()?,
        }
        // Blocks in the order they are written: the pages, the index bottom up, the nodes. A page's or index block's
        // goes into the pointer to it until its sum is known.
        let mut b = start;
        if pages {
            let mut p = self.next_dirty(0);
            while let Some(q) = p {
                let c = self.claim(&mut b);
                let (s, at) = self.page_entry(q);
                self.cache[s][at..at + 8].copy_from_slice(&c.0.to_le_bytes());
                p = self.next_dirty(q + 1);
            }
            for l in 1..=self.ix_h {
                let mut i = self.next_ix(l, 0);
                while let Some(j) = i {
                    let c = self.claim(&mut b);
                    match self.ix_parent(l, j) {
                        Some((s, at)) => {
                            self.cache[s][at..at + 8].copy_from_slice(&c.0.to_le_bytes())
                        }
                        None => self.ix.0 = c,
                    }
                    i = self.next_ix(l, j + 1);
                }
            }
        }
        for s in self.base..self.top {
            if self.dirt[s] {
                self.blk[s] = self.claim(&mut b);
            }
        }
        self.finalize(generation);
        if pages {
            let words = self.blocks.div_ceil(64) as usize;
            let mut p = self.next_dirty(0);
            while let Some(q) = p {
                let d = self.stage(&mut first, &mut n)?;
                let len = 8 * min(PAGE_WORDS, words - q * PAGE_WORDS);
                let page = &mut self.cache[d];
                for (i, word) in self.bits[q * PAGE_WORDS..][..len / 8].iter().enumerate() {
                    page[8 * i..8 * i + 8].copy_from_slice(&word.to_le_bytes());
                }
                page[len..].fill(0);
                let (s, at) = self.page_entry(q);
                let c = Block(le64(&self.cache[s], at));
                let sum = checksum(c, &self.cache[d][..len]);
                self.cache[s][at + 8..at + 16].copy_from_slice(&sum.0.to_le_bytes());
                self.blk[d] = c;
                p = self.next_dirty(q + 1);
            }
            for l in 1..=self.ix_h {
                let mut i = self.next_ix(l, 0);
                while let Some(j) = i {
                    let d = self.stage(&mut first, &mut n)?;
                    let slot = self.ix_level(l).0 + j;
                    let len = 16 * ix_children(self.pages, l, j);
                    let parent = self.ix_parent(l, j);
                    let c = match parent {
                        Some((s, at)) => Block(le64(&self.cache[s], at)),
                        None => self.ix.0,
                    };
                    let sum = seal(c, &mut self.cache[slot], len);
                    let (src, dst) = pair(self.cache, slot, d);
                    dst.copy_from_slice(src);
                    self.blk[d] = c;
                    match parent {
                        Some((s, at)) => {
                            self.cache[s][at + 8..at + 16].copy_from_slice(&sum.0.to_le_bytes())
                        }
                        None => self.ix.1 = sum,
                    }
                    i = self.next_ix(l, j + 1);
                }
            }
        }
        self.write_out(first, n)?;
        self.flush()?;
        let list = 16 * self.list_entries();
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
            self.root.block.0,
            self.root.sum.0,
            self.root.generation,
            self.height as u64 - 1,
            if pages { 0 } else { self.nlog as u64 },
            self.ix_h as u64,
        ];
        for (i, f) in fields.iter().enumerate() {
            sb[8 * i..8 * i + 8].copy_from_slice(&f.to_le_bytes());
        }
        if self.ix_h == 0 {
            sb[SB_HDR..SB_HDR + list].copy_from_slice(&self.cache[self.top][..list]);
        } else {
            sb[SB_HDR..SB_HDR + 8].copy_from_slice(&self.ix.0.0.to_le_bytes());
            sb[SB_HDR + 8..SB_HDR + 16].copy_from_slice(&self.ix.1.0.to_le_bytes());
        }
        if !pages {
            for (j, &i) in self.log[..self.nlog].iter().enumerate() {
                let at = SB_HDR + list + 16 * j;
                sb[at..at + 8].copy_from_slice(&i.to_le_bytes());
                sb[at + 8..at + 16].copy_from_slice(&self.bits[i as usize].to_le_bytes());
            }
        }
        seal(Block(generation % 2), sb, END);
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
                &mut self.bits[COMMITTED * w + i],
            );
            let before = (l | *c).count_ones();
            *c = n | l;
            self.free = self.free + before as u64 - (l | *c).count_ones() as u64;
            self.bits[NEWEST * w + i] = l;
        }
        (self.prev_span, self.span) = (self.span, (usize::MAX, 0));
        if pages {
            let d = 3 * w;
            self.bits[d..d + self.pages.div_ceil(64)].fill(0);
            (self.nlog, self.full) = (0, false);
        }
        self.hint = Block(0);
        self.changed = false;
        Ok(())
    }

    /// Sizes the memory for a disk of `blocks` and drops every cached node.
    fn setup(&mut self, blocks: u64) -> Result<(), Error> {
        self.broken = true;
        let pages = pages(blocks);
        let base = 1 + STAGE;
        let pool = min(
            self.cache.len().saturating_sub(base + ix_blocks(pages)),
            MAX_POOL,
        );
        if pool < MIN_POOL || self.bits.len() < bitmap_words(blocks) {
            return Err(Error::TooBig);
        }
        (self.blocks, self.pages, self.words) = (blocks, pages, pages * PAGE_WORDS);
        (self.base, self.top, self.ix_h) = (base, base + pool, ix_height(pages));
        self.hint = Block(0);
        (self.nlog, self.full) = (0, false);
        self.blk.fill(EMPTY);
        self.dirt.fill(false);
        self.ndirty = 0;
        let d = 3 * self.words;
        self.bits[d..d + pages.div_ceil(64)].fill(0);
        (self.span, self.prev_span) = ((usize::MAX, 0), (0, pages * PAGE_WORDS));
        (self.cached, self.unwritten, self.items) = (None, false, [None; 2]);
        (self.finger, self.start) = (None, (NONE, 0));
        self.forget_names();
        Ok(())
    }

    /// Reads `s`'s bitmap into `map` (the words of this disk's size): its index, its pages, then its log. For `LIVE`
    /// it keeps the page list in `cache[top..]`, the log's words in `log` and the words they replace in the last
    /// staging slot; another slot takes a page it shares with the live list from those, and its index blocks through
    /// the staging slots one level each. The bitmap must mark its pages, its index blocks and the root.
    fn load_bitmap(&mut self, s: &Super, map: usize) -> Result<(), Error> {
        let (live, sb) = (map == LIVE, (s.generation % 2) as usize);
        let (pages, w) = (pages(s.blocks), self.words);
        let words = s.blocks.div_ceil(64) as usize;
        let saved = self.base - 1;
        if live {
            self.ix = (Block(0), Sum(0));
            if s.ix_h == 0 {
                let list = 16 * pages;
                let top = self.top;
                let (sbuf, list_buf) = (&self.bufs[sb], &mut self.cache[top]);
                list_buf[..list].copy_from_slice(&sbuf[SB_HDR..SB_HDR + list]);
                list_buf[list..].fill(0);
            } else {
                self.ix = s.ix;
                let root = self.ix_level(s.ix_h).0;
                self.read_ix(root, s.ix, 16 * ix_children(pages, s.ix_h, 0), s.blocks)?;
                for l in (1..s.ix_h).rev() {
                    let (at, n) = self.ix_level(l);
                    for i in 0..n {
                        let (ps, pat) = self.ix_parent(l, i).unwrap_or((0, 0));
                        let ptr = (
                            Block(le64(&self.cache[ps], pat)),
                            Sum(le64(&self.cache[ps], pat + 8)),
                        );
                        self.read_ix(at + i, ptr, 16 * ix_children(pages, l, i), s.blocks)?;
                    }
                }
            }
        }
        self.bits[map * w..(map + 1) * w].fill(0);
        let mut held = [usize::MAX; MAX_IX + 1];
        for p in 0..pages {
            let (b, sum) = self.list_entry(s, live, &mut held, p, None)?;
            if b == Block(0) && sum == Sum(0) {
                continue;
            }
            if !(2..s.blocks).contains(&b.0) {
                return Err(Error::Corrupt);
            }
            if !live && p < self.pages && self.list_entry(s, true, &mut held, p, None)? == (b, sum)
            {
                let lo = p * PAGE_WORDS;
                let hi = min(lo + PAGE_WORDS, w);
                self.bits.copy_within(lo..hi, map * w + lo);
                for j in 0..self.nlog {
                    let i = self.log[j] as usize;
                    if (lo..hi).contains(&i) {
                        self.bits[map * w + i] = le64(&self.cache[saved], 8 * j);
                    }
                }
                continue;
            }
            self.disk.read(b.0, from_mut(&mut self.cache[0]))?;
            let (page, n) = (&self.cache[0], min(PAGE_WORDS, words - p * PAGE_WORDS));
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
        let at = SB_HDR + 16 * if s.ix_h == 0 { pages } else { 1 };
        for j in 0..s.log {
            let (i, v) = (
                le64(&self.bufs[sb], at + 16 * j),
                le64(&self.bufs[sb], at + 16 * j + 8),
            );
            // A log word past this disk's size (the other slot may claim more) marks no block it allocates.
            let Some(i) = Some(i as usize).filter(|&i| i < w) else {
                continue;
            };
            if live {
                let old = self.bits[i].to_le_bytes();
                self.cache[saved][8 * j..8 * j + 8].copy_from_slice(&old);
                self.log[j] = i as u64;
            }
            self.bits[map * w + i] = v;
        }
        if live {
            self.nlog = s.log;
        }
        // Every page and index block the list reaches, and the superblocks and the root, are marked.
        let mut held = [usize::MAX; MAX_IX + 1];
        for p in 0..pages {
            let (b, _) = self.list_entry(s, live, &mut held, p, Some(map))?;
            if b != Block(0) && !self.marked(map, b) {
                return Err(Error::Corrupt);
            }
        }
        if !(self.marked(map, Block(0))
            && self.marked(map, Block(1))
            && self.marked(map, s.root.block))
        {
            return Err(Error::Corrupt);
        }
        if live {
            for l in 1..=s.ix_h {
                for i in 0..ix_count(pages, l) {
                    let b = match self.ix_parent(l, i) {
                        Some((ps, pat)) => Block(le64(&self.cache[ps], pat)),
                        None => self.ix.0,
                    };
                    if b != Block(0) && !self.marked(map, b) {
                        return Err(Error::Corrupt);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether `map` marks `b`; another slot's map holds only the words its own pages gave, the rest are the live
    /// ones. Blocks past this disk's size are never allocated, so need no mark.
    fn marked(&self, map: usize, b: Block) -> bool {
        let (w, i) = (self.words, (b.0 / 64) as usize);
        b.0 >= self.blocks
            || (self.bits[map * w + i] | if map == LIVE { 0 } else { self.bits[i] }) >> (b.0 % 64)
                & 1
                != 0
    }

    /// Reads index block `ptr` into slot `slot`; it lists `len` bytes of entries. A zero pointer is an all-zero block.
    fn read_ix(
        &mut self,
        slot: usize,
        ptr: (Block, Sum),
        len: usize,
        blocks: u64,
    ) -> Result<(), Error> {
        if ptr == (Block(0), Sum(0)) {
            self.cache[slot].fill(0);
            return Ok(());
        }
        if !(2..blocks).contains(&ptr.0.0) {
            return Err(Error::Corrupt);
        }
        self.disk.read(ptr.0.0, from_mut(&mut self.cache[slot]))?;
        let n = &self.cache[slot];
        if Sum(le64(n, END)) != ptr.1
            || checksum(ptr.0, &n[..len]) != ptr.1
            || n[len..END].iter().any(|&b| b != 0)
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Page `p`'s (block, sum) in slot `s`'s list: the live one from `cache[top..]`, another from its superblock or
    /// by reading its index down from the root, one level per staging slot (`held` notes which block each holds).
    /// With `check`, each index block read must be marked in that map.
    fn list_entry(
        &mut self,
        s: &Super,
        live: bool,
        held: &mut [usize; MAX_IX + 1],
        p: usize,
        check: Option<usize>,
    ) -> Result<(Block, Sum), Error> {
        let sb = (s.generation % 2) as usize;
        let (slot, at) = if live {
            self.page_entry(p)
        } else if s.ix_h == 0 {
            (usize::MAX, SB_HDR + 16 * p)
        } else {
            let pages = pages(s.blocks);
            let mut ptr = s.ix;
            for l in (1..=s.ix_h).rev() {
                let i = (0..l).fold(p, |i, _| i / FAN);
                if held[l] != i {
                    self.read_ix(l, ptr, 16 * ix_children(pages, l, i), s.blocks)?;
                    if let Some(map) = check
                        && ptr.0 != Block(0)
                        && !self.marked(map, ptr.0)
                    {
                        return Err(Error::Corrupt);
                    }
                    held[l] = i;
                    held[..l].fill(usize::MAX);
                }
                let at = 16 * ((0..l - 1).fold(p, |i, _| i / FAN) % FAN);
                ptr = (
                    Block(le64(&self.cache[l], at)),
                    Sum(le64(&self.cache[l], at + 8)),
                );
            }
            return Ok(ptr);
        };
        let b = if slot == usize::MAX {
            &self.bufs[sb]
        } else {
            &self.cache[slot]
        };
        Ok((Block(le64(b, at)), Sum(le64(b, at + 8))))
    }

    /// The counter exceeds every inode number: the last item down the rightmost path has the largest.
    fn check_counter(&mut self) -> Result<(), Error> {
        let (mut p, mut level, mut lo) = (self.root, self.height - 1, Key(0));
        loop {
            let s = self.node(p, level, lo, NONE)?;
            let n = &self.cache[s];
            let c = count(n);
            if level == 0 {
                if c > 0 && ikey(n, c - 1).inode().0 >= self.next_inode {
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

    /// `inode`'s item, from the memo of the last two read or else the tree. A reference, since the kernel's dev build
    /// instantiates this at opt-level 1, where each move of an `Item` is a `memcpy` call.
    fn inode(&mut self, inode: Inode) -> Result<&Item, Error> {
        let at = match &self.items {
            [Some((i, _)), _] if *i == inode => 0,
            [_, Some((i, _))] if *i == inode => 1,
            _ => {
                let k = Key::new(inode, ItemKind::Inode, Offset(0));
                let (s, _) = self.leaf(k)?;
                let n = &self.cache[s];
                let i = search(n, k);
                if i == count(n) || ikey(n, i) != k {
                    return Err(Error::NotFound);
                }
                self.items = [Some((inode, decode(value(n, i)))), self.items[0]];
                0
            }
        };
        match &self.items[at] {
            Some((_, it)) => Ok(it),
            None => Err(Error::NotFound),
        }
    }

    fn file(&mut self, inode: Inode) -> Result<&Item, Error> {
        let it = self.inode(inode)?;
        if it.kind == DIR {
            return Err(Error::IsDir);
        }
        Ok(it)
    }

    fn dir(&mut self, inode: Inode) -> Result<&Item, Error> {
        let it = self.inode(inode)?;
        if it.kind != DIR {
            return Err(Error::NotDir);
        }
        Ok(it)
    }

    /// The inode entry `e` of `dir` names, if it records that entry back; `Corrupt` if not.
    fn child(&mut self, dir: Inode, e: Entry) -> Result<&Item, Error> {
        let it = self.inode(e.1).map_err(|_| Error::Corrupt)?;
        if it.parent != dir || it.entry != e.0 || it.kind != e.2 {
            return Err(Error::Corrupt);
        }
        Ok(it)
    }

    fn forget_names(&mut self) {
        for j in 0..NAMES {
            self.names[j].1 = 0;
        }
    }

    fn set_inode(&mut self, inode: Inode, it: &Item) -> Result<(), Error> {
        self.items = [None; 2];
        let (s, at) = self.value_mut(Key::new(inode, ItemKind::Inode, Offset(0)))?;
        encode(&mut self.cache[s][at..at + INODE_LEN], it);
        Ok(())
    }

    /// `name`'s entry in `dir` (its offset, inode and kind), and the first free offset in its hash chain.
    fn find_entry(
        &mut self,
        dir: Inode,
        name: &[u8],
    ) -> Result<(Option<Entry>, Option<Offset>), Error> {
        let base = name_hash(self.seed, name);
        let (mut k, last) = (
            Key::new(dir, ItemKind::Entry, base),
            Key::new(dir, ItemKind::Entry, base + (CHAIN - 1)),
        );
        let mut used = 0u8;
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                let ik = ikey(n, i);
                if ik > last {
                    break;
                }
                let v = value(n, i);
                if &v[9..] == name {
                    return Ok((Some((ik.offset(), Inode(le64(v, 0)), v[8])), None));
                }
                used |= 1 << (ik.offset() - base);
            }
            if hi > last {
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
        let mut d = *self.dir(dir)?;
        let (found, slot) = self.find_entry(dir, name)?;
        match found {
            Some(e) if kind == FILE => return self.child(dir, e).map(|_| e.1),
            Some(_) => return Err(Error::Exists),
            None => {}
        }
        if self.broken {
            return Err(Error::Io);
        }
        let slot = slot.ok_or(Error::Collision)?;
        self.forget_names();
        let inode = Inode(self.next_inode);
        let next = self.next_inode.checked_add(1).ok_or(Error::NoSpace)?;
        self.reserve(0, 3, ITEM + 9 + name.len() + ITEM + INODE_LEN, false)?;
        let it = Item {
            kind,
            mode: if kind == DIR { 0o755 } else { 0o644 },
            links: 1,
            size: 0,
            parent: dir,
            entry: slot,
            mtime: self.now,
            ctime: self.now,
            btime: self.now,
        };
        self.next_inode = next;
        let r = self
            .put_entry(dir, slot, inode, kind, name)
            .and_then(|()| self.insert(Key::new(inode, ItemKind::Inode, Offset(0)), INODE_LEN))
            .and_then(|(s, at)| {
                encode(&mut self.cache[s][at..at + INODE_LEN], &it);
                (d.mtime, d.ctime) = (self.now, self.now);
                self.set_inode(dir, &d)
            });
        self.broken |= r.is_err();
        r.map(|()| inode)
    }

    fn put_entry(
        &mut self,
        dir: Inode,
        off: Offset,
        inode: Inode,
        kind: u8,
        name: &[u8],
    ) -> Result<(), Error> {
        let (s, at) = self.insert(Key::new(dir, ItemKind::Entry, off), 9 + name.len())?;
        let v = &mut self.cache[s][at..at + 9 + name.len()];
        v[..8].copy_from_slice(&inode.0.to_le_bytes());
        v[8] = kind;
        v[9..].copy_from_slice(name);
        Ok(())
    }

    /// Whether `target` is `dir` or below it, walking up parents; a parent cycle is `Corrupt` (Brent's algorithm).
    fn below(&mut self, dir: Inode, target: Inode) -> Result<bool, Error> {
        let (mut hare, mut tortoise, mut power, mut steps) = (target, target, 1u64, 0u64);
        loop {
            if hare == dir {
                return Ok(true);
            }
            if hare == ROOT {
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

    fn write_pages(&mut self, inode: Inode, offset: u64, data: &[u8]) -> Result<(), Error> {
        let end = offset + data.len() as u64;
        let mut pos = offset;
        while pos < end {
            let (page, at) = (
                Page(pos / BLOCK_SIZE as u64),
                (pos % BLOCK_SIZE as u64) as usize,
            );
            let n = min(BLOCK_SIZE - at, (end - pos) as usize);
            let old = self.extent_at(inode, page)?;
            let old_block = old.map(|(off, start, _)| start + (page - off));
            let held = self
                .cached
                .is_some_and(|(.., i, p)| i == inode && p == page);
            if !held {
                self.write_data()?;
            }
            match old {
                _ if held => {}
                Some((off, start, _)) if n < BLOCK_SIZE => {
                    let sum = Sum(le64(
                        &self.bufs[META],
                        8 + 8 * ((page - off) % EXTENT_MAX) as usize,
                    ));
                    self.load_page(start + (page - off), sum, inode, page)?;
                }
                _ => self.bufs[DATA].fill(0),
            }
            (self.cached, self.unwritten) = (None, false);
            self.bufs[DATA][at..at + n].copy_from_slice(&data[(pos - offset) as usize..][..n]);
            let b = match old_block {
                Some(b) if !self.has(COMMITTED, b) => b,
                _ => self.alloc(1)?,
            };
            let sum = checksum(b, &self.bufs[DATA]);
            self.set_page(inode, page, b, sum, old)?;
            (self.cached, self.unwritten) = (Some((b, sum, inode, page)), true);
            pos += n as u64;
        }
        Ok(())
    }

    /// Maps `page` of `inode` to block `b` with `sum`; `old` is the extent that covered it, its value in scratch.
    fn set_page(
        &mut self,
        inode: Inode,
        page: Page,
        b: Block,
        sum: Sum,
        old: Option<(Page, Block, u64)>,
    ) -> Result<(), Error> {
        if let Some((off, start, c)) = old {
            let j = (page - off) % EXTENT_MAX;
            if start + j == b {
                let (s, at) = self.value_mut(extent_key(inode, off))?;
                self.cache[s][at + 8 + 8 * j as usize..][..8].copy_from_slice(&sum.0.to_le_bytes());
                return Ok(());
            }
            self.release(start + j)?;
            self.delete(extent_key(inode, off))?;
            let j = j as usize;
            if j + 1 < c as usize {
                // The tail's value: its first block over the replaced page's sum, then the sums after it.
                self.bufs[META][8 + 8 * j..16 + 8 * j]
                    .copy_from_slice(&(start + (j as u64 + 1)).0.to_le_bytes());
                self.put_item(extent_key(inode, page + 1), 8 + 8 * j, 8 + 8 * c as usize)?;
            }
            if j > 0 {
                self.put_item(extent_key(inode, off), 0, 8 + 8 * j)?;
            }
        }
        if page.0 > 0
            && let Some((off, start, c)) = self.extent_at(inode, Page(page.0 - 1))?
            && off + c == page
            && start + c == b
            && c < EXTENT_MAX
        {
            let c = c as usize;
            self.bufs[META][8 + 8 * c..16 + 8 * c].copy_from_slice(&sum.0.to_le_bytes());
            self.delete(extent_key(inode, off))?;
            return self.put_item(extent_key(inode, off), 0, 16 + 8 * c);
        }
        self.bufs[META][..8].copy_from_slice(&b.0.to_le_bytes());
        self.bufs[META][8..16].copy_from_slice(&sum.0.to_le_bytes());
        self.put_item(extent_key(inode, page), 0, 16)
    }

    /// Inserts an item whose value is `bufs[META][from..to]`.
    fn put_item(&mut self, k: Key, from: usize, to: usize) -> Result<(), Error> {
        let (s, at) = self.insert(k, to - from)?;
        self.cache[s][at..at + to - from].copy_from_slice(&self.bufs[META][from..to]);
        Ok(())
    }

    /// The extent of `inode` covering `page` (first page, first block, pages), its value copied to `bufs[META]`.
    fn extent_at(&mut self, inode: Inode, page: Page) -> Result<Option<(Page, Block, u64)>, Error> {
        let mut k = extent_key(inode, Page(page.0.saturating_sub(EXTENT_MAX - 1)));
        let last = extent_key(inode, page);
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                if ikey(n, i) > last {
                    return Ok(None);
                }
                let (off, v) = (Page(ikey(n, i).offset().0), value(n, i));
                let c = (v.len() as u64 - 8) / 8;
                if page < off + c {
                    self.bufs[META][..v.len()].copy_from_slice(v);
                    return Ok(Some((off, Block(le64(v, 0)), c)));
                }
            }
            if hi > last {
                return Ok(None);
            }
            k = hi;
        }
    }

    /// Bytes of `inode`'s extent items.
    fn extent_bytes(&mut self, inode: Inode) -> Result<usize, Error> {
        let mut k = extent_key(inode, Page(0));
        let last = Key::new(inode, ItemKind::Extent, Offset(OFFSET));
        let mut bytes = 0;
        loop {
            let (s, hi) = self.leaf(k)?;
            let n = &self.cache[s];
            for i in search(n, k)..count(n) {
                if ikey(n, i) > last {
                    return Ok(bytes);
                }
                bytes += ITEM + value(n, i).len();
            }
            if hi > last {
                return Ok(bytes);
            }
            k = hi;
        }
    }

    fn remove_extents(&mut self, inode: Inode) -> Result<(), Error> {
        let first = extent_key(inode, Page(0));
        let last = Key::new(inode, ItemKind::Extent, Offset(OFFSET));
        while let Some((s, i, _)) = self.seek(first)? {
            let n = &self.cache[s];
            let k = ikey(n, i);
            if k > last {
                break;
            }
            let v = value(n, i);
            let (start, c) = (Block(le64(v, 0)), (v.len() as u64 - 8) / 8);
            for j in 0..c {
                self.release(start + j)?;
            }
            self.delete(k)?;
        }
        Ok(())
    }

    /// Fails with `NoSpace` unless `data` blocks, the nodes `paths` tree changes touching `bytes` of items may dirty
    /// or add, and what commit needs (every dirty node, the bitmap pages and index blocks) are free. A change that is not
    /// a `removal` also leaves room for one, so a full disk can still be emptied.
    fn reserve(&self, data: u64, paths: usize, bytes: usize, removal: bool) -> Result<(), Error> {
        // Adjacent leaves together hold at least a quarter leaf, and internal nodes are at least a quarter full.
        let nodes = |paths: usize, bytes: usize| {
            2 * (paths * (self.height + 1) + 2 * bytes.div_ceil(QUARTER))
        };
        let floor = if removal { 0 } else { nodes(4, 0) };
        let ix = if self.ix_h == 0 {
            0
        } else {
            ix_blocks(self.pages)
        };
        let need = data + (nodes(paths, bytes) + floor + self.ndirty + self.pages + ix) as u64;
        if need > self.free {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    fn has(&self, map: usize, b: Block) -> bool {
        self.bits[map * self.words + (b.0 / 64) as usize] >> (b.0 % 64) & 1 != 0
    }

    fn used(&self, b: Block) -> bool {
        self.has(LIVE, b) || self.has(COMMITTED, b)
    }

    /// A block in range that the live tree reaches.
    fn live(&self, b: Block) -> bool {
        (2..self.blocks).contains(&b.0) && self.has(LIVE, b)
    }

    fn mark(&mut self, b: Block) {
        self.bits[(b.0 / 64) as usize] |= 1 << (b.0 % 64);
        self.touch(b);
        self.free -= 1;
        self.changed = true;
    }

    /// Notes that `b`'s live bit changed.
    fn touch(&mut self, b: Block) {
        let (p, i) = ((b.0 / PAGE_BITS) as usize, (b.0 / 64) as usize);
        self.bits[3 * self.words + p / 64] |= 1 << (p % 64);
        self.span = (min(self.span.0, i), self.span.1.max(i + 1));
        if self.full {
            return;
        }
        if let Err(at) = self.log[..self.nlog].binary_search(&(i as u64)) {
            if self.nlog == self.log_cap() {
                self.full = true;
            } else {
                self.log.copy_within(at..self.nlog, at + 1);
                (self.log[at], self.nlog) = (i as u64, self.nlog + 1);
            }
        }
    }

    /// Entries the superblock's page list takes: each page's, or the index root's.
    fn list_entries(&self) -> usize {
        if self.ix_h == 0 { self.pages } else { 1 }
    }

    /// Log entries the superblock has room for after the page list.
    fn log_cap(&self) -> usize {
        (END - SB_HDR) / 16 - self.list_entries()
    }

    fn page_dirty(&self, p: usize) -> bool {
        self.bits[3 * self.words + p / 64] >> (p % 64) & 1 != 0
    }

    /// The first page from `p` on changed since the pages were written.
    fn next_dirty(&self, p: usize) -> Option<usize> {
        let d = 3 * self.words;
        let mut i = p / 64;
        let mut w = self.bits.get(d + i)? & (!0u64 << (p % 64));
        loop {
            if w != 0 {
                return Some(64 * i + w.trailing_zeros() as usize).filter(|&q| q < self.pages);
            }
            i += 1;
            if 64 * i >= self.pages {
                return None;
            }
            w = self.bits[d + i];
        }
    }

    /// The first index block from `i` on at level `l` above a changed page.
    fn next_ix(&self, l: usize, i: usize) -> Option<usize> {
        let span = (0..l).fold(1usize, |n, _| n * FAN);
        let q = self.next_dirty(i.checked_mul(span)?)?;
        Some(q / span)
    }

    /// Level `l` of the live index (1: the blocks listing pages): its first cache slot and block count.
    fn ix_level(&self, l: usize) -> (usize, usize) {
        let mut at = self.top;
        for j in 1..l {
            at += ix_count(self.pages, j);
        }
        (at, ix_count(self.pages, l))
    }

    /// Where page `p`'s (block, sum) sits: a cache slot and offset.
    fn page_entry(&self, p: usize) -> (usize, usize) {
        if self.ix_h == 0 {
            (self.top, 16 * p)
        } else {
            (self.top + p / FAN, 16 * (p % FAN))
        }
    }

    /// Where the pointer to live index block `i` at level `l` sits; `None` for the root.
    fn ix_parent(&self, l: usize, i: usize) -> Option<(usize, usize)> {
        (l < self.ix_h).then(|| (self.ix_level(l + 1).0 + i / FAN, 16 * (i % FAN)))
    }

    /// Releases the block a list entry points to, if any, and zeroes the entry.
    fn release_entry(&mut self, s: usize, at: usize) -> Result<(), Error> {
        let b = Block(le64(&self.cache[s], at));
        if b != Block(0) {
            self.release(b)?;
        }
        self.cache[s][at..at + 16].fill(0);
        Ok(())
    }

    /// The next staging slot, writing the staged ones out first if none is left.
    fn stage(&mut self, first: &mut usize, n: &mut usize) -> Result<usize, Error> {
        if *n == self.base {
            self.write_slots(*first, *n)?;
            (*first, *n) = (1, 1);
        }
        *n += 1;
        Ok(*n - 1)
    }

    /// A free block, the first of `k` free ones if there is such a run.
    fn alloc(&mut self, k: usize) -> Result<Block, Error> {
        let b = self.start(k);
        if b.0 >= self.blocks {
            return Err(Error::NoSpace);
        }
        if b == self.next_free(self.hint) {
            self.hint = b + 1;
        }
        self.mark(b);
        Ok(b)
    }

    /// The first free block at or after `b` (`blocks` if none).
    fn next_free(&self, b: Block) -> Block {
        let (w, mut b) = (self.words, b.0);
        while b < self.blocks {
            let i = (b / 64) as usize;
            let used = (self.bits[i] | self.bits[COMMITTED * w + i]) >> (b % 64);
            if used == !0 >> (b % 64) {
                b = (b | 63) + 1;
            } else {
                return Block(b + used.trailing_ones() as u64);
            }
        }
        Block(self.blocks)
    }

    /// Moves the unwritten data page to free block `b` if an extent of its own maps it; false if not.
    fn lead_data(&mut self, b: Block) -> Result<bool, Error> {
        let Some((d, _, inode, page)) = self.cached else {
            return Ok(false);
        };
        // Its extent's path may need new dirty nodes; a commit must never run short of space.
        if self.reserve(1, 1, 0, true).is_err() {
            return Ok(false);
        }
        let Some((off, _, 1)) = self.extent_at(inode, page)? else {
            return Ok(false);
        };
        let (s, at) = self.value_mut(extent_key(inode, off))?;
        let sum = checksum(b, &self.bufs[DATA]);
        self.cache[s][at..at + 8].copy_from_slice(&b.0.to_le_bytes());
        self.cache[s][at + 8..at + 16].copy_from_slice(&sum.0.to_le_bytes());
        if b == self.next_free(self.hint) {
            self.hint = b + 1;
        }
        self.mark(b);
        (self.cached, self.unwritten) = (None, false);
        self.release(d)?;
        (self.cached, self.unwritten) = (Some((b, sum, inode, page)), true);
        Ok(true)
    }

    /// Whether the `k` blocks from `b` on are free.
    fn run_free(&self, b: Block, k: usize) -> bool {
        (b.0..b.0 + k as u64).all(|c| c < self.blocks && !self.used(Block(c)))
    }

    /// The start of the first run of `k` free blocks, or of the first free block if no run is that long.
    fn start(&self, k: usize) -> Block {
        let first = self.next_free(self.hint);
        let mut b = first;
        while b.0 < self.blocks {
            let mut end = b;
            while end.0 < self.blocks && end - b < k as u64 && !self.used(end) {
                end = end + 1;
            }
            if end - b == k as u64 {
                return b;
            }
            b = self.next_free(end);
        }
        first
    }

    /// `b` left the live tree: free at once if no slot reaches it, else once the commit after next replaces them.
    fn release(&mut self, b: Block) -> Result<(), Error> {
        if !self.live(b) {
            return Err(Error::Corrupt);
        }
        self.bits[(b.0 / 64) as usize] &= !(1 << (b.0 % 64));
        self.touch(b);
        if !self.has(COMMITTED, b) {
            self.free += 1;
            self.hint = min(self.hint, b);
        }
        if self.cached.is_some_and(|(c, ..)| c == b) {
            (self.cached, self.unwritten) = (None, false);
        }
        self.changed = true;
        Ok(())
    }

    fn load_page(&mut self, b: Block, sum: Sum, inode: Inode, page: Page) -> Result<(), Error> {
        if self.cached == Some((b, sum, inode, page)) {
            return Ok(());
        }
        self.write_data()?;
        self.cached = None;
        self.disk.read(b.0, from_mut(&mut self.bufs[DATA]))?;
        verify(b, &self.bufs[DATA], sum)?;
        self.cached = Some((b, sum, inode, page));
        Ok(())
    }

    /// Writes the data page in `bufs[DATA]` to its block if it is not written yet.
    fn write_data(&mut self) -> Result<(), Error> {
        if let (Some((b, ..)), true) = (self.cached, self.unwritten) {
            let r = self.disk.write(b.0, from_ref(&self.bufs[DATA]));
            self.broken |= r.is_err();
            r?;
            self.unwritten = false;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        let r = self.disk.flush();
        self.broken |= r.is_err();
        r
    }

    /// The cache slot holding the node `p` points to at `level`, read and checked against the bounds if not cached.
    fn node(&mut self, p: Ptr, level: usize, lo: Key, hi: Key) -> Result<usize, Error> {
        self.clock += 1;
        if let Some(s) = p.slot() {
            return Ok(s);
        }
        if let Some(s) = (self.base..self.top).find(|&s| self.blk[s] == p.block) {
            if self.cache[s][0] as usize != level {
                return Err(Error::Corrupt);
            }
            self.stamp[s] = self.clock;
            return Ok(s);
        }
        let s = self.victim();
        (self.finger, self.start) = (None, (NONE, 0));
        self.blk[s] = EMPTY;
        self.disk.read(p.block.0, from_mut(&mut self.cache[s]))?;
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
    fn check(&self, s: usize, p: Ptr, level: usize, lo: Key, hi: Key) -> Result<(), Error> {
        let n = &self.cache[s];
        let c = count(n);
        let bad = Sum(le64(n, END)) != p.sum
            || checksum(p.block, &n[..END]) != p.sum
            || n[0] as usize != level
            || n[1] != 0
            || n[6..HDR] != [0; 2]
            || (level > 0 && n[4..6] != [0; 2]);
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
        let root = level == self.height - 1 && lo == Key(0) && hi == NONE;
        if c * ITEM > CAP || (c == 0 && !root) {
            return Err(Error::Corrupt);
        }
        if !packed(n) {
            return Err(Error::Corrupt);
        }
        let mut prev_end = None;
        for i in 0..c {
            let (k, off, len) = (ikey(n, i), voff(n, i), vlen(n, i));
            if k < lo || k >= hi || prev.is_some_and(|q| k <= q) {
                return Err(Error::Corrupt);
            }
            prev = Some(k);
            let (inode, o) = (k.inode(), k.offset().0);
            let v = &n[off..off + len];
            let ok = match k.kind() {
                Some(ItemKind::Inode) => {
                    o == 0
                        && len == INODE_LEN
                        && matches!(v[0], FILE | DIR)
                        && v[1] == 0
                        && le16(v, 2) <= 0o7777
                        && le64(v, 8) <= MAX_FILE_SIZE
                        && le64(v, 24) <= OFFSET
                }
                Some(ItemKind::Entry) => {
                    (10..=9 + NAME_MAX).contains(&len)
                        && Inode(le64(v, 0)) != ROOT
                        && Inode(le64(v, 0)) != inode
                        && matches!(v[8], FILE | DIR)
                        && valid_name(&v[9..])
                }
                Some(ItemKind::Extent) => {
                    let (pages, start) = ((len as u64).saturating_sub(8) / 8, Block(le64(v, 0)));
                    let ok = len % 8 == 0
                        && (1..=EXTENT_MAX).contains(&pages)
                        && o + pages <= MAX_FILE_SIZE / BLOCK_SIZE as u64
                        && prev_end.is_none_or(|e| e <= k)
                        && extent_key(inode, Page(o + pages - 1)) < hi
                        && start.0 < self.blocks
                        && (0..pages).all(|j| self.live(start + j));
                    if ok {
                        prev_end = Some(extent_key(inode, Page(o + pages)));
                    }
                    ok
                }
                None => false,
            };
            if !ok {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }

    /// The leaf whose range holds `k`, and the leaf's upper bound (`NONE` for the last), without changing anything.
    fn leaf(&mut self, k: Key) -> Result<(usize, Key), Error> {
        if let Some((s, lo, hi)) = self.finger
            && lo <= k
            && k < hi
        {
            self.clock += 1;
            self.stamp[s] = self.clock;
            return Ok((s, hi));
        }
        let (mut p, mut level, mut lo, mut hi) = (self.root, self.height - 1, Key(0), NONE);
        loop {
            let s = self.node(p, level, lo, hi)?;
            if level == 0 {
                self.finger = Some((s, lo, hi));
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
    fn seek(&mut self, mut k: Key) -> Result<Option<(usize, usize, Key)>, Error> {
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

    /// Makes the path to `k`'s leaf dirty, recorded in `path`, writing dirty nodes out first if the cache is short of
    /// slots.
    fn cow(&mut self, k: Key, path: &mut Path) -> Result<(), Error> {
        if self.top - self.base - self.ndirty < 3 * (self.height + 2) {
            self.spill()?;
        }
        let (mut p, mut level, mut lo, mut hi) = (self.root, self.height - 1, Key(0), NONE);
        let mut parent: Option<(usize, usize)> = None;
        loop {
            let s = self.node(p, level, lo, hi)?;
            self.make_dirty(s)?;
            match parent {
                None => self.root = tagged(s),
                Some((ps, i)) => set_eblock(&mut self.cache[ps], i, tagged(s).block),
            }
            (path.slot[level], path.lo[level], path.hi[level]) = (s, lo, hi);
            if level == 0 {
                return Ok(());
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
        (self.finger, self.start) = (None, (NONE, 0));
        self.cache[s][..HDR].copy_from_slice(&[level as u8, 0, 0, 0, 0, 0, 0, 0]);
        if level == 0 {
            set_bottom(&mut self.cache[s], END);
        }
        (self.blk[s], self.dirt[s]) = (EMPTY, true);
        self.ndirty += 1;
        self.changed = true;
        s
    }

    /// Frees node slot `s` and the block of the node it held.
    fn drop_node(&mut self, s: usize) -> Result<(), Error> {
        (self.finger, self.start) = (None, (NONE, 0));
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
    fn value_mut(&mut self, k: Key) -> Result<(usize, usize), Error> {
        let mut path = Path::default();
        self.cow(k, &mut path)?;
        let s = path.slot[0];
        let n = &self.cache[s];
        let i = search(n, k);
        if i == count(n) || ikey(n, i) != k {
            return Err(Error::Corrupt);
        }
        Ok((s, voff(n, i)))
    }

    /// Adds item `k` with a `len`-byte value to fill; returns its slot and value offset.
    fn insert(&mut self, k: Key, len: usize) -> Result<(usize, usize), Error> {
        (self.finger, self.start) = (None, (NONE, 0));
        let mut path = Path::default();
        self.cow(k, &mut path)?;
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
        mut k: Key,
        mut child: usize,
    ) -> Result<(), Error> {
        loop {
            if level == self.height {
                if self.height == MAX_HEIGHT {
                    return Err(Error::TooBig);
                }
                let r = self.new_node(level);
                let old = path.slot[level - 1];
                internal_insert(&mut self.cache[r], 0, Key(0), tagged(old));
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
    fn delete(&mut self, k: Key) -> Result<(), Error> {
        (self.finger, self.start) = (None, (NONE, 0));
        let mut path = Path::default();
        self.cow(k, &mut path)?;
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
                set_eblock(&mut self.cache[parent], j, tagged(sib).block);
                (sib, s, i)
            };
            let at = count(&self.cache[l]);
            let (a, b) = pair(self.cache, r, l);
            if level == 0 {
                leaf_move(a, 0, b);
            } else {
                internal_move(a, 0, b);
                let sep = ekey(&self.cache[parent], ri);
                self.cache[l][HDR + ENTRY * at..][..16].copy_from_slice(&sep.0.to_le_bytes());
            }
            self.drop_node(r)?;
            internal_remove(&mut self.cache[parent], ri);
        }
        while self.height > 1
            && let Some(r) = self.root.slot()
        {
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
    fn claim(&mut self, b: &mut Block) -> Block {
        let c = self.next_free(*b);
        self.mark(c);
        *b = c + 1;
        c
    }

    /// Seals the dirty nodes bottom up, filling each parent's pointers with its children's blocks and sums.
    fn finalize(&mut self, generation: u64) {
        for level in 0..self.height {
            for s in self.base..self.top {
                if !self.dirt[s] || self.cache[s][0] as usize != level {
                    continue;
                }
                for i in 0..if level > 0 { count(&self.cache[s]) } else { 0 } {
                    if let Some(c) = eptr(&self.cache[s], i).slot() {
                        let p = Ptr {
                            block: self.blk[c],
                            sum: Sum(le64(&self.cache[c], END)),
                            generation,
                        };
                        set_eptr(&mut self.cache[s], i, p);
                    }
                }
                seal(self.blk[s], &mut self.cache[s], END);
            }
        }
        if let Some(s) = self.root.slot() {
            self.root = Ptr {
                block: self.blk[s],
                sum: Sum(le64(&self.cache[s], END)),
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
            let r = self.disk.write(self.blk[a].0, &self.cache[a..b]);
            self.broken |= r.is_err();
            r?;
            a = b;
        }
        Ok(())
    }
}

fn tagged(s: usize) -> Ptr {
    Ptr {
        block: Block(TAG | s as u64),
        sum: Sum(0),
        generation: 0,
    }
}

#[inline(always)]
fn kind_of(kind: u8) -> Kind {
    if kind == DIR { Kind::Dir } else { Kind::File }
}

/// Two distinct slots of `cache`, mutably.
fn pair(cache: &mut [Buf], a: usize, b: usize) -> (&mut Buf, &mut Buf) {
    if a < b {
        let (x, y) = cache.split_at_mut(b);
        (&mut x[a], &mut y[0])
    } else {
        let (x, y) = cache.split_at_mut(a);
        (&mut y[0], &mut x[b])
    }
}

/// Slot `slot`'s superblock if its sum, magic and generation hold; `valid` if every other field does too.
fn superblock(sb: &Buf, slot: u64, disk: u64) -> Option<Super> {
    let f = |i: usize| le64(sb, 8 * i);
    if Sum(le64(sb, END)) != checksum(Block(slot), &sb[..END]) || f(0) != MAGIC || f(1) % 2 != slot
    {
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
            block: Block(f(9)),
            sum: Sum(f(10)),
            generation: f(11),
        },
        level: f(12) as usize,
        log: 0,
        ix_h: f(14).min(MAX_IX as u64 + 1) as usize,
        ix: (Block(f(15)), Sum(f(16))),
    };
    let (pages, words) = (pages(s.blocks.min(MAX_BLOCKS)), s.blocks.div_ceil(64));
    let list = if s.ix_h == 0 { min(pages, INLINE) } else { 1 };
    let log = f(13).min(LOG_MAX as u64 + 1) as usize;
    let end = SB_HDR + 16 * (list + log);
    let at = SB_HDR + 16 * list;
    // Entries in increasing word order within the disk's size, the last word's bits past it clear.
    let log_ok = end <= END
        && (0..log).all(|j| {
            let (i, v) = (le64(sb, at + 16 * j), le64(sb, at + 16 * j + 8));
            (j == 0 || le64(sb, at + 16 * j - 16) < i)
                && i < words
                && (i + 1 < words || s.blocks.is_multiple_of(64) || v >> (s.blocks % 64) == 0)
        });
    s.valid = (MIN_BLOCKS..=disk).contains(&s.blocks)
        && s.next_inode >= 1
        && f(6) == 0
        && f(7) == 0
        && f(8) == 1
        && f(12) < MAX_HEIGHT as u64
        && s.root.generation <= s.generation
        && (2..s.blocks).contains(&s.root.block.0)
        && s.blocks <= MAX_BLOCKS
        && s.ix_h == ix_height(pages)
        && (s.ix_h == 0 || (2..s.blocks).contains(&s.ix.0.0))
        && log_ok
        && sb[end..END].iter().all(|&b| b == 0);
    s.log = if s.valid { log } else { 0 };
    Some(s)
}

fn encode(v: &mut [u8], it: &Item) {
    v[0] = it.kind;
    v[1] = 0;
    v[2..4].copy_from_slice(&it.mode.to_le_bytes());
    v[4..8].copy_from_slice(&it.links.to_le_bytes());
    for (i, f) in [
        it.size,
        it.parent.0,
        it.entry.0,
        it.mtime,
        it.ctime,
        it.btime,
    ]
    .iter()
    .enumerate()
    {
        v[8 + 8 * i..16 + 8 * i].copy_from_slice(&f.to_le_bytes());
    }
}

#[inline(always)]
fn decode(v: &[u8]) -> Item {
    Item {
        kind: v[0],
        mode: le16(v, 2) as u16,
        links: u32::from_le_bytes(v[4..8].try_into().unwrap()),
        size: le64(v, 8),
        parent: Inode(le64(v, 16)),
        entry: Offset(le64(v, 24)),
        mtime: le64(v, 32),
        ctime: le64(v, 40),
        btime: le64(v, 48),
    }
}

#[inline(always)]
fn count(n: &[u8]) -> usize {
    le16(n, 2)
}

fn set_count(n: &mut [u8], c: usize) {
    n[2..4].copy_from_slice(&(c as u16).to_le_bytes());
}

#[inline(always)]
fn ikey(n: &[u8], i: usize) -> Key {
    Key(le128(n, HDR + ITEM * i))
}

#[inline(always)]
fn voff(n: &[u8], i: usize) -> usize {
    le16(n, HDR + ITEM * i + 16)
}

#[inline(always)]
fn vlen(n: &[u8], i: usize) -> usize {
    le16(n, HDR + ITEM * i + 18)
}

#[inline(always)]
fn value(n: &[u8], i: usize) -> &[u8] {
    &n[voff(n, i)..voff(n, i) + vlen(n, i)]
}

#[inline(always)]
fn ekey(n: &[u8], i: usize) -> Key {
    Key(le128(n, HDR + ENTRY * i))
}

#[inline(always)]
fn eptr(n: &[u8], i: usize) -> Ptr {
    let at = HDR + ENTRY * i;
    Ptr {
        block: Block(le64(n, at + 16)),
        sum: Sum(le64(n, at + 24)),
        generation: le64(n, at + 32),
    }
}

fn set_eptr(n: &mut [u8], i: usize, p: Ptr) {
    let at = HDR + ENTRY * i + 16;
    for (j, f) in [p.block.0, p.sum.0, p.generation].iter().enumerate() {
        n[at + 8 * j..at + 8 * j + 8].copy_from_slice(&f.to_le_bytes());
    }
}

fn set_eblock(n: &mut [u8], i: usize, block: Block) {
    n[HDR + ENTRY * i + 16..][..8].copy_from_slice(&block.0.to_le_bytes());
}

/// Bytes a node's items or entries take (with their values).
fn used(n: &[u8]) -> usize {
    let c = count(n);
    match (n[0], c) {
        (0, 0) => 0,
        (0, _) => ITEM * c + END - bottom(n),
        _ => ENTRY * c,
    }
}

/// The first item at or after `k`.
#[inline(always)]
fn search(n: &[u8], k: Key) -> usize {
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
#[inline(always)]
fn route(n: &[u8], k: Key) -> usize {
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

/// The offset of a leaf's lowest value (`END` with none).
fn bottom(n: &[u8]) -> usize {
    le16(n, 4)
}

fn set_bottom(n: &mut [u8], b: usize) {
    n[4..6].copy_from_slice(&(b as u16).to_le_bytes());
}

fn set_voff(n: &mut [u8], i: usize, off: usize) {
    let at = HDR + ITEM * i + 16;
    n[at..at + 2].copy_from_slice(&(off as u16).to_le_bytes());
}

/// Whether a leaf's values lie between its item array and `END`, filling the bytes from its lowest one up without
/// overlap.
fn packed(n: &[u8]) -> bool {
    let (c, b) = (count(n), bottom(n));
    let mut used = [0u64; BLOCK_SIZE / 64];
    let mut total = 0;
    if b < HDR + ITEM * c || b > END {
        return false;
    }
    for i in 0..c {
        let (off, len) = (voff(n, i), vlen(n, i));
        if off < b || off + len > END {
            return false;
        }
        let mut at = off;
        while at < off + len {
            let (w, lo) = (at / 64, at % 64);
            let hi = min(64, off + len - 64 * w);
            let mask = (u64::MAX >> (64 - (hi - lo))) << lo;
            if used[w] & mask != 0 {
                return false;
            }
            used[w] |= mask;
            at = 64 * w + hi;
        }
        total += len;
    }
    total == END - b
}

/// Opens a `len`-byte value for item `k` at index `i`, below the others; returns its offset. The leaf must have room.
fn leaf_insert(n: &mut [u8], i: usize, k: Key, len: usize) -> usize {
    let (c, off) = (count(n), bottom(n) - len);
    n.copy_within(HDR + ITEM * i..HDR + ITEM * c, HDR + ITEM * (i + 1));
    let at = HDR + ITEM * i;
    n[at..at + 16].copy_from_slice(&k.0.to_le_bytes());
    n[at + 18..at + 20].copy_from_slice(&(len as u16).to_le_bytes());
    set_voff(n, i, off);
    set_count(n, c + 1);
    set_bottom(n, off);
    off
}

/// Removes item `i`; the values below it move up into its place.
fn leaf_remove(n: &mut [u8], i: usize) {
    let (c, b) = (count(n), bottom(n));
    let (off, len) = (voff(n, i), vlen(n, i));
    n.copy_within(b..off, b + len);
    n.copy_within(HDR + ITEM * (i + 1)..HDR + ITEM * c, HDR + ITEM * i);
    if off > b {
        for j in 0..c - 1 {
            let o = voff(n, j);
            if o < off {
                set_voff(n, j, o + len);
            }
        }
    }
    set_count(n, c - 1);
    set_bottom(n, b + len);
}

/// Appends `src`'s items from `from` on to `dst`, which must have room, and drops them from `src`, whose values pack
/// up again.
fn leaf_move(src: &mut [u8], from: usize, dst: &mut [u8]) {
    for i in from..count(src) {
        let len = vlen(src, i);
        let at = leaf_insert(dst, count(dst), ikey(src, i), len);
        dst[at..at + len].copy_from_slice(value(src, i));
    }
    set_count(src, from);
    // Highest value first, each moved up to just below the last: it never reaches a lower one not yet moved.
    let (mut top, mut below) = (END, END);
    loop {
        let next = (0..from)
            .filter(|&i| voff(src, i) < below)
            .max_by_key(|&i| voff(src, i));
        let Some(i) = next else { break };
        let (off, len) = (voff(src, i), vlen(src, i));
        src.copy_within(off..off + len, top - len);
        set_voff(src, i, top - len);
        (top, below) = (top - len, off);
    }
    set_bottom(src, top);
}

fn internal_insert(n: &mut [u8], i: usize, k: Key, p: Ptr) {
    let c = count(n);
    n.copy_within(HDR + ENTRY * i..HDR + ENTRY * c, HDR + ENTRY * (i + 1));
    n[HDR + ENTRY * i..][..16].copy_from_slice(&k.0.to_le_bytes());
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
        && name.iter().all(|&b| b != b'/' && b != 0)
}

fn mix(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29)
}

/// The first offset of `name`'s hash chain: a seeded multiply-rotate hash with a final mix.
fn name_hash(seed: u64, name: &[u8]) -> Offset {
    let (words, rest) = name.as_chunks::<8>();
    let mut h = words.iter().fold(seed ^ name.len() as u64, |h, w| {
        mix(h, u64::from_le_bytes(*w))
    });
    if !rest.is_empty() {
        h = mix(h, rest.iter().rev().fold(0, |w, &b| w << 8 | b as u64));
    }
    h ^= h >> 31;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 29;
    Offset((h >> 5) << 3)
}

/// Sixteen interleaved multiply-rotate lanes over 64-bit words; each step is a bijection, so any one-word change shows.
fn checksum(block: Block, buf: &[u8]) -> Sum {
    let (words, _) = buf.as_chunks::<8>();
    let mut lanes: [u64; 16] = core::array::from_fn(|i| i as u64);
    lanes[0] ^= block.0 | 1 << 63;
    for chunk in words.chunks(16) {
        for (l, w) in lanes.iter_mut().zip(chunk) {
            *l = mix(*l, u64::from_le_bytes(*w));
        }
    }
    Sum(lanes.into_iter().fold(0, mix))
}

/// Writes the sum of `block` and `n`'s first `len` bytes into its last 8; returns it.
fn seal(block: Block, n: &mut Buf, len: usize) -> Sum {
    let sum = checksum(block, &n[..len]);
    n[END..].copy_from_slice(&sum.0.to_le_bytes());
    sum
}

#[inline(always)]
fn le16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

#[inline(always)]
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[inline(always)]
fn le128(b: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(b[at..at + 16].try_into().unwrap())
}

#[cfg(test)]
mod tests;
