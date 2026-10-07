//! MogFS: a checksummed copy-on-write file system over 4 KiB blocks.
//!
//! Format (little-endian):
//! - Every block ends with a 64-bit hash of its block number and its first 4088 bytes (the payload); a mismatch
//!   reads as `Error::Corrupt`.
//! - Blocks 0 and 1 are superblock slots; generation `g` goes to slot `g % 2`, and mount takes the valid slot with
//!   the highest generation. Superblock: magic u64, generation u64, block count u32, then the root: the inode table's
//!   8 block numbers (0 = a block of free inodes).
//! - Inode table block: 63 records of 64 bytes: kind u8 (0 free, 1 file, 2 directory), 3 zero bytes, size u32, 14
//!   data block numbers (0 = zeros). Inode `n` is record `n % 63` of table block `n / 63`; inode 0 is the root
//!   directory. Inode numbers stay fixed while their blocks move.
//! - File data: byte `i` lives at payload offset `i % 4088` of data block `i / 4088`.
//! - Directory data: 56-byte entries (inode u32, name length u8, name), 73 per block, in creation order.
//! - Copy-on-write: no block reachable from either slot is written. A change writes its data blocks at once, each to
//!   a new block or over one allocated since the last commit; changed table blocks wait for `commit`, which writes
//!   them, flushes, writes the other slot with the next generation, and flushes.
//! - Free space is not stored: mount derives it from both slots' tables (a block reached twice in one slot is
//!   corrupt), and the blocks reachable from either slot stay reserved, so a fallback mount finds its tree intact.
#![cfg_attr(not(test), no_std)]

use core::cmp::{max, min};
use core::slice::{from_mut, from_ref};

pub const BLOCK_SIZE: usize = 4096;
/// Largest file system, in blocks (64 MiB); `format` uses at most this much of a bigger disk.
pub const MAX_BLOCKS: u64 = 16384;
pub const MAX_INODES: u32 = (TABLE_BLOCKS * PER_TABLE) as u32;
pub const MAX_FILE_SIZE: u64 = (PTRS * PAYLOAD) as u64;
pub const NAME_MAX: usize = DIRENT - 5;
/// The root directory.
pub const ROOT: Inode = Inode(0);

const PAYLOAD: usize = BLOCK_SIZE - 8;
/// Two superblock slots and one inode table block.
const MIN_BLOCKS: u32 = 3;
const MAGIC: u64 = u64::from_le_bytes(*b"MogFS\0\0\x01");
const TABLE_BLOCKS: usize = 8;
const RECORD: usize = 64;
const PER_TABLE: usize = PAYLOAD / RECORD;
const PTRS: usize = 14;
const DIRENT: usize = 56;
const PER_DIR_BLOCK: usize = PAYLOAD / DIRENT;
const FREE: u8 = 0;
const FILE: u8 = 1;
const DIR: u8 = 2;
const WORDS: usize = MAX_BLOCKS as usize / 64;

type Bitmap = [u64; WORDS];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    Corrupt,
    NotFound,
    Exists,
    NotDir,
    IsDir,
    InvalidName,
    TooBig,
    NoSpace,
}

/// A block device of 4 KiB blocks; a request covers `bufs.len()` consecutive blocks from `block`.
pub trait Disk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error>;
    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error>;
    /// Returns once every completed write is durable.
    fn flush(&mut self) -> Result<(), Error>;
    fn blocks(&self) -> u64;
}

/// A file or directory, stable across writes and commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Clone, Copy)]
struct Record {
    kind: u8,
    size: u32,
    ptrs: [u32; PTRS],
}

impl Record {
    const EMPTY: Self = Self {
        kind: FREE,
        size: 0,
        ptrs: [0; PTRS],
    };
}

/// A file system on `D`, about 48 KiB: keep it in a static or on the heap. `Io` from any change leaves it refusing
/// writes and commits until the next `mount`; `Io` from `commit` means the commit may or may not be durable.
pub struct Fs<D> {
    disk: D,
    blocks: u32,
    generation: u64,
    /// The working inode table: a dirty block already has the block number it will be written to.
    table: [u32; TABLE_BLOCKS],
    dirty: u8,
    records: [Record; MAX_INODES as usize],
    /// Blocks reachable from the newest slot; from either slot or a superblock; also allocated since the last commit;
    /// in the newest slot but no longer in the working tree.
    newest: Bitmap,
    committed: Bitmap,
    used: Bitmap,
    replaced: Bitmap,
    free: u32,
    /// Every word of `used` before it is full.
    hint: usize,
    buf: [u8; BLOCK_SIZE],
    /// The data block `buf` holds unchanged.
    cached: Option<u32>,
    meta: [u8; BLOCK_SIZE],
    broken: bool,
}

impl<D: Disk> Fs<D> {
    /// An unmounted file system; `mount` or `format` it before use.
    pub const fn new(disk: D) -> Self {
        Self {
            disk,
            blocks: 0,
            generation: 0,
            table: [0; TABLE_BLOCKS],
            dirty: 0,
            records: [Record::EMPTY; MAX_INODES as usize],
            newest: [0; WORDS],
            committed: [0; WORDS],
            used: [0; WORDS],
            replaced: [0; WORDS],
            free: 0,
            hint: 0,
            buf: [0; BLOCK_SIZE],
            cached: None,
            meta: [0; BLOCK_SIZE],
            broken: false,
        }
    }

    /// Writes an empty file system (only a root directory) and commits it.
    pub fn format(&mut self) -> Result<(), Error> {
        self.reset();
        if self.blocks < MIN_BLOCKS {
            return Err(Error::NoSpace);
        }
        // Stale superblocks could outrank the new ones.
        self.meta.fill(0);
        self.put(0, true)?;
        self.put(1, true)?;
        self.flush()?;
        self.reserve(0, &[ROOT])?;
        let root = Record {
            kind: DIR,
            ..Record::EMPTY
        };
        self.set(ROOT, root)?;
        self.commit()
    }

    /// Loads the newest valid slot, or the older one if the newest's table is corrupt; drops uncommitted changes.
    pub fn mount(&mut self) -> Result<(), Error> {
        self.reset();
        let (a, b) = (self.superblock(0), self.superblock(1));
        if a == Err(Error::Io) || b == Err(Error::Io) {
            return Err(Error::Io);
        }
        let mut slots = [a.ok(), b.ok()];
        if slots[0].map(|s| s.0) < slots[1].map(|s| s.0) {
            slots.swap(0, 1);
        }
        for (i, slot) in slots.into_iter().enumerate() {
            let Some((generation, blocks, table)) = slot else {
                continue;
            };
            self.blocks = blocks;
            match self.load_slot(table, false) {
                Ok(()) => {}
                Err(Error::Corrupt) => {
                    self.records.fill(Record::EMPTY);
                    self.newest.fill(0);
                    continue;
                }
                Err(e) => return Err(e),
            }
            (self.generation, self.table) = (generation, table);
            if i == 0
                && let Some((_, older_blocks, older)) = slots[1]
                && older_blocks == blocks
            {
                // A damaged older slot only loses its fallback.
                match self.load_slot(older, true) {
                    Ok(()) => {}
                    Err(Error::Corrupt) => self.committed.fill(0),
                    Err(e) => return Err(e),
                }
            }
            for (c, n) in self.committed.iter_mut().zip(&self.newest) {
                *c |= n;
            }
            self.committed[0] |= 0b11;
            self.used = self.committed;
            self.count_free();
            return Ok(());
        }
        Err(Error::Corrupt)
    }

    pub fn lookup(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.scan(dir, false, |n, _| n == name)?
            .ok_or(Error::NotFound)
    }

    /// Calls `f` with each entry's name and inode, in creation order.
    pub fn readdir(&mut self, dir: Inode, mut f: impl FnMut(&[u8], Inode)) -> Result<(), Error> {
        self.scan(dir, true, |n, i| {
            f(n, i);
            false
        })
        .map(|_| ())
    }

    pub fn kind(&self, inode: Inode) -> Result<Kind, Error> {
        match self.records[inode.0 as usize].kind {
            FILE => Ok(Kind::File),
            DIR => Ok(Kind::Dir),
            _ => Err(Error::NotFound),
        }
    }

    pub fn mkdir(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        self.add(dir, name, DIR)
    }

    pub fn create(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        self.add(dir, name, FILE)
    }

    /// Reads from `offset` up to the end of the file; returns the byte count (0 at or past the end).
    pub fn read(&mut self, file: Inode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let r = self.file(file)?;
        let end = min(r.size as u64, offset.saturating_add(buf.len() as u64));
        let mut pos = offset;
        while pos < end {
            let (i, at) = (
                (pos / PAYLOAD as u64) as usize,
                (pos % PAYLOAD as u64) as usize,
            );
            let n = min(PAYLOAD - at, (end - pos) as usize);
            let out = &mut buf[(pos - offset) as usize..][..n];
            match r.ptrs[i] {
                0 => out.fill(0),
                p => {
                    self.load(p, false)?;
                    out.copy_from_slice(&self.buf[at..at + n]);
                }
            }
            pos += n as u64;
        }
        Ok(end.saturating_sub(offset) as usize)
    }

    /// Writes `data` at `offset`, growing the file; a gap past the old end reads as zeros. `NoSpace` changes nothing.
    pub fn write(&mut self, file: Inode, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut r = self.file(file)?;
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&e| e <= MAX_FILE_SIZE)
            .ok_or(Error::TooBig)?;
        if data.is_empty() {
            return Ok(());
        }
        let span = (offset / PAYLOAD as u64) as usize..=((end - 1) / PAYLOAD as u64) as usize;
        let need = span.filter(|&i| !self.fresh(r.ptrs[i])).count();
        self.reserve(need, &[file])?;
        self.write_data(&mut r, offset, data)?;
        self.set(file, r)
    }

    /// Empties `file`.
    pub fn truncate(&mut self, file: Inode) -> Result<(), Error> {
        self.file(file)?;
        self.reserve(0, &[file])?;
        let r = Record {
            kind: FILE,
            ..Record::EMPTY
        };
        self.set(file, r)
    }

    /// Makes every change so far durable, atomically; does nothing if nothing changed.
    pub fn commit(&mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        if self.dirty == 0 {
            return Ok(());
        }
        let dirty = self.dirty;
        for t in (0..TABLE_BLOCKS).filter(|t| dirty & 1 << t != 0) {
            self.encode(t);
            self.store(self.table[t], true)?;
        }
        self.flush()?;
        let generation = self.generation + 1;
        let sb = &mut self.meta;
        sb.fill(0);
        sb[..8].copy_from_slice(&MAGIC.to_le_bytes());
        sb[8..16].copy_from_slice(&generation.to_le_bytes());
        sb[16..20].copy_from_slice(&self.blocks.to_le_bytes());
        for (i, t) in self.table.iter().enumerate() {
            sb[20 + 4 * i..][..4].copy_from_slice(&t.to_le_bytes());
        }
        self.store((generation % 2) as u32, true)?;
        self.flush()?;
        self.generation = generation;
        self.dirty = 0;
        let maps = self.newest.iter_mut().zip(&mut self.committed);
        for (((newest, committed), used), replaced) in
            maps.zip(&mut self.used).zip(&mut self.replaced)
        {
            let reach = (*newest & !*replaced) | (*used & !*committed);
            *committed = reach | *newest;
            *newest = reach;
            *used = *committed;
            *replaced = 0;
        }
        self.committed[0] |= 0b11;
        self.used[0] |= 0b11;
        self.count_free();
        Ok(())
    }

    fn reset(&mut self) {
        self.blocks = min(self.disk.blocks(), MAX_BLOCKS) as u32;
        self.generation = 0;
        self.table = [0; TABLE_BLOCKS];
        self.dirty = 0;
        self.records.fill(Record::EMPTY);
        self.newest.fill(0);
        self.committed.fill(0);
        self.used.fill(0);
        self.replaced.fill(0);
        self.committed[0] = 0b11;
        self.used[0] = 0b11;
        self.count_free();
        self.cached = None;
        self.broken = false;
    }

    fn count_free(&mut self) {
        let used: u32 = self.used.iter().map(|w| w.count_ones()).sum();
        self.free = self.blocks.saturating_sub(used);
        self.hint = 0;
    }

    /// Generation, block count and inode table of a valid superblock slot.
    fn superblock(&mut self, slot: u32) -> Result<(u64, u32, [u32; TABLE_BLOCKS]), Error> {
        self.load(slot, true)?;
        let (generation, blocks) = (le64(&self.meta, 8), le32(&self.meta, 16));
        let table = core::array::from_fn(|i| le32(&self.meta, 20 + 4 * i));
        if le64(&self.meta, 0) != MAGIC
            || generation == u64::MAX
            || generation % 2 != slot as u64
            || !(MIN_BLOCKS..=self.blocks).contains(&blocks)
            || !valid(&table, blocks)
        {
            return Err(Error::Corrupt);
        }
        Ok((generation, blocks, table))
    }

    /// Marks the blocks `table` reaches in `newest` (and loads its records) or, for the older slot, in `committed`.
    fn load_slot(&mut self, table: [u32; TABLE_BLOCKS], older: bool) -> Result<(), Error> {
        for (t, b) in table.into_iter().enumerate().filter(|&(_, b)| b != 0) {
            self.reach(b, older)?;
            self.load(b, true)?;
            for i in 0..PER_TABLE {
                let r = self.decode(i)?;
                if r.kind != FREE {
                    for p in r.ptrs.into_iter().filter(|&p| p != 0) {
                        self.reach(p, older)?;
                    }
                }
                if !older {
                    self.records[t * PER_TABLE + i] = r;
                }
            }
        }
        Ok(())
    }

    fn reach(&mut self, b: u32, older: bool) -> Result<(), Error> {
        let map = if older {
            &mut self.committed
        } else {
            &mut self.newest
        };
        let (w, bit) = (b as usize / 64, 1 << (b % 64));
        if map[w] & bit != 0 {
            return Err(Error::Corrupt);
        }
        map[w] |= bit;
        Ok(())
    }

    /// Fails with `NoSpace` unless `blocks` data blocks plus the clean table blocks of `inodes` can be allocated.
    fn reserve(&self, blocks: usize, inodes: &[Inode]) -> Result<(), Error> {
        let tables = inodes
            .iter()
            .fold(0u8, |m, i| m | 1 << (i.0 as usize / PER_TABLE));
        if blocks + (tables & !self.dirty).count_ones() as usize > self.free as usize {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    fn alloc(&mut self) -> Result<u32, Error> {
        let w = self.hint
            + self.used[self.hint..]
                .iter()
                .position(|&w| w != !0)
                .ok_or(Error::NoSpace)?;
        let b = (w * 64) as u32 + self.used[w].trailing_ones();
        if b >= self.blocks {
            return Err(Error::NoSpace);
        }
        self.used[w] |= 1 << (b % 64);
        self.free -= 1;
        self.hint = w;
        Ok(b)
    }

    /// Allocated since the last commit, so no slot reaches it.
    fn fresh(&self, b: u32) -> bool {
        b != 0 && self.committed[b as usize / 64] & (1 << (b % 64)) == 0
    }

    /// `b` left the working tree: free at once if fresh, else once the commit after next replaces its slot.
    fn release(&mut self, b: u32) {
        let (w, bit) = (b as usize / 64, 1 << (b % 64));
        if self.fresh(b) {
            self.used[w] &= !bit;
            self.free += 1;
            self.hint = min(self.hint, w);
        } else {
            self.replaced[w] |= bit;
        }
    }

    /// Replaces `inode`'s record; its table block is written at commit. Call `reserve` first.
    fn set(&mut self, inode: Inode, r: Record) -> Result<(), Error> {
        let (i, t) = (inode.0 as usize, inode.0 as usize / PER_TABLE);
        let old = self.records[i];
        if self.dirty & 1 << t == 0 {
            if self.table[t] != 0 {
                self.release(self.table[t]);
            }
            self.table[t] = self.alloc()?;
            self.dirty |= 1 << t;
        }
        for (&o, &n) in old.ptrs.iter().zip(&r.ptrs) {
            if o != 0 && o != n {
                self.release(o);
            }
        }
        self.records[i] = r;
        Ok(())
    }

    fn load(&mut self, b: u32, meta: bool) -> Result<(), Error> {
        if !meta {
            if self.cached == Some(b) {
                return Ok(());
            }
            self.cached = None;
        }
        let buf = if meta { &mut self.meta } else { &mut self.buf };
        self.disk.read(b as u64, from_mut(buf))?;
        if le64(buf, PAYLOAD) != checksum(b, buf) {
            return Err(Error::Corrupt);
        }
        if !meta {
            self.cached = Some(b);
        }
        Ok(())
    }

    /// Seals `meta` (table and superblock blocks) or `buf` (data blocks, which then stay cached) and writes it to `b`.
    fn store(&mut self, b: u32, meta: bool) -> Result<(), Error> {
        if !meta || self.cached == Some(b) {
            self.cached = None;
        }
        let buf = if meta { &mut self.meta } else { &mut self.buf };
        let sum = checksum(b, buf);
        buf[PAYLOAD..].copy_from_slice(&sum.to_le_bytes());
        self.put(b, meta)?;
        if !meta {
            self.cached = Some(b);
        }
        Ok(())
    }

    fn put(&mut self, b: u32, meta: bool) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let buf = if meta { &self.meta } else { &self.buf };
        let r = self.disk.write(b as u64, from_ref(buf));
        // A fresh block rewritten in place may be torn.
        self.broken |= r.is_err();
        r
    }

    fn flush(&mut self) -> Result<(), Error> {
        let r = self.disk.flush();
        self.broken |= r.is_err();
        r
    }

    /// Record `i` of the table block in `meta`.
    fn decode(&self, i: usize) -> Result<Record, Error> {
        let b = &self.meta[i * RECORD..][..RECORD];
        let r = Record {
            kind: b[0],
            size: le32(b, 4),
            ptrs: core::array::from_fn(|p| le32(b, 8 + 4 * p)),
        };
        if r.kind > DIR
            || r.size as u64 > MAX_FILE_SIZE
            || (r.kind == DIR && !(r.size as usize).is_multiple_of(DIRENT))
            || !valid(&r.ptrs, self.blocks)
        {
            return Err(Error::Corrupt);
        }
        Ok(r)
    }

    /// Table block `t` into `meta`.
    fn encode(&mut self, t: usize) {
        self.meta.fill(0);
        for (i, r) in self.records[t * PER_TABLE..][..PER_TABLE]
            .iter()
            .enumerate()
        {
            let b = &mut self.meta[i * RECORD..][..RECORD];
            b[0] = r.kind;
            b[4..8].copy_from_slice(&r.size.to_le_bytes());
            for (p, ptr) in r.ptrs.iter().enumerate() {
                b[8 + 4 * p..][..4].copy_from_slice(&ptr.to_le_bytes());
            }
        }
    }

    fn file(&self, inode: Inode) -> Result<Record, Error> {
        let r = self.records[inode.0 as usize];
        match r.kind {
            FILE => Ok(r),
            DIR => Err(Error::IsDir),
            _ => Err(Error::NotFound),
        }
    }

    fn dir(&self, inode: Inode) -> Result<Record, Error> {
        let r = self.records[inode.0 as usize];
        match r.kind {
            DIR => Ok(r),
            FILE => Err(Error::NotDir),
            _ => Err(Error::NotFound),
        }
    }

    /// Writes the data blocks of `r` (a copy; the table is untouched) and grows its size. Call `reserve` first.
    fn write_data(&mut self, r: &mut Record, offset: u64, data: &[u8]) -> Result<(), Error> {
        let end = offset + data.len() as u64;
        let mut pos = offset;
        while pos < end {
            let (i, at) = (
                (pos / PAYLOAD as u64) as usize,
                (pos % PAYLOAD as u64) as usize,
            );
            let n = min(PAYLOAD - at, (end - pos) as usize);
            let old = r.ptrs[i];
            if old != 0
                && n < PAYLOAD
                && let Err(e) = self.load(old, false)
            {
                // Blocks already stored for this write are neither referenced nor released.
                self.broken |= pos > offset;
                return Err(e);
            }
            self.cached = None;
            if old == 0 {
                self.buf.fill(0);
            }
            self.buf[at..at + n].copy_from_slice(&data[(pos - offset) as usize..][..n]);
            let b = if self.fresh(old) { old } else { self.alloc()? };
            self.store(b, false)?;
            r.ptrs[i] = b;
            pos += n as u64;
        }
        r.size = max(r.size, end as u32);
        Ok(())
    }

    /// The first entry of `dir` for which `f` returns true; `names` checks each name, which a valid name to match never needs.
    fn scan(
        &mut self,
        dir: Inode,
        names: bool,
        mut f: impl FnMut(&[u8], Inode) -> bool,
    ) -> Result<Option<Inode>, Error> {
        let r = self.dir(dir)?;
        for e in 0..r.size as usize / DIRENT {
            let at = e % PER_DIR_BLOCK * DIRENT;
            if at == 0 {
                match r.ptrs[e / PER_DIR_BLOCK] {
                    0 => return Err(Error::Corrupt),
                    p => self.load(p, false)?,
                }
            }
            let d = &self.buf[at..at + DIRENT];
            let (inode, len) = (le32(d, 0), d[4] as usize);
            if inode >= MAX_INODES || len > NAME_MAX || (names && !valid_name(&d[5..5 + len])) {
                return Err(Error::Corrupt);
            }
            if f(&d[5..5 + len], Inode(inode)) {
                return Ok(Some(Inode(inode)));
            }
        }
        Ok(None)
    }

    fn add(&mut self, dir: Inode, name: &[u8], kind: u8) -> Result<Inode, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        if self.scan(dir, false, |n, _| n == name)?.is_some() {
            return Err(Error::Exists);
        }
        let mut d = self.dir(dir)?;
        if d.size as u64 + DIRENT as u64 > MAX_FILE_SIZE {
            return Err(Error::TooBig);
        }
        let inode = self
            .records
            .iter()
            .position(|r| r.kind == FREE)
            .ok_or(Error::NoSpace)?;
        let inode = Inode(inode as u32);
        // Entries never straddle blocks, so the new one needs at most its own block.
        let need = !self.fresh(d.ptrs[d.size as usize / PAYLOAD]) as usize;
        self.reserve(need, &[dir, inode])?;
        let mut entry = [0; DIRENT];
        entry[..4].copy_from_slice(&inode.0.to_le_bytes());
        entry[4] = name.len() as u8;
        entry[5..5 + name.len()].copy_from_slice(name);
        let end = d.size as u64;
        self.write_data(&mut d, end, &entry)?;
        self.set(dir, d)?;
        let r = Record {
            kind,
            ..Record::EMPTY
        };
        self.set(inode, r)?;
        Ok(inode)
    }
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != b"."
        && name != b".."
        && !name.contains(&b'/')
        && !name.contains(&0)
}

/// Every pointer is 0 (none) or a block past the superblocks.
fn valid(ptrs: &[u32], blocks: u32) -> bool {
    ptrs.iter().all(|&p| p == 0 || (2..blocks).contains(&p))
}

/// Sixteen interleaved multiply-rotate lanes over 64-bit words; each step is a bijection, so any one-word change shows.
fn checksum(block: u32, buf: &[u8; BLOCK_SIZE]) -> u64 {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    let (words, _) = buf[..PAYLOAD].as_chunks::<8>();
    let mut lanes: [u64; 16] = core::array::from_fn(|i| i as u64);
    lanes[0] ^= block as u64 | 1 << 40;
    for chunk in words.chunks(16) {
        for (l, w) in lanes.iter_mut().zip(chunk) {
            *l = mix(*l, u64::from_le_bytes(*w));
        }
    }
    lanes.into_iter().fold(0, mix)
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
