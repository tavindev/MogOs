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
//! - Copy-on-write: no block reachable from either slot is written. A change writes its data blocks and its table
//!   block, each to a new block or over one allocated since the last commit; `commit` is flush, the other slot with
//!   the next generation, flush.
//! - Free space is not stored: mount reads both slots' tables into memory, and the blocks reachable from either slot
//!   stay reserved, so a fallback mount always finds its tree intact.
#![cfg_attr(not(test), no_std)]

use core::cmp::{max, min};

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

/// A block device of 4 KiB blocks.
pub trait Disk {
    fn read(&mut self, block: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), Error>;
    fn write(&mut self, block: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), Error>;
    /// Returns once every completed write is durable.
    fn flush(&mut self) -> Result<(), Error>;
    fn blocks(&self) -> u64;
}

/// A file or directory, stable across writes and commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode(u32);

#[derive(Clone, Copy, Default)]
struct Record {
    kind: u8,
    size: u32,
    ptrs: [u32; PTRS],
}

/// A mounted file system. After an `Io` error it refuses to commit; mount again to continue from the last commit.
pub struct Fs<D> {
    disk: D,
    blocks: u32,
    generation: u64,
    /// The working inode table, written through on every change.
    table: [u32; TABLE_BLOCKS],
    records: [Record; MAX_INODES as usize],
    /// Blocks reachable from the newest slot; from either slot; from either slot or allocated since.
    newest: Bitmap,
    committed: Bitmap,
    used: Bitmap,
    buf: [u8; BLOCK_SIZE],
    /// The block `buf` holds unchanged.
    cached: Option<u32>,
    meta: [u8; BLOCK_SIZE],
    broken: bool,
}

impl<D: Disk> Fs<D> {
    /// Writes an empty file system (only a root directory) and commits it.
    pub fn format(disk: D) -> Result<Self, Error> {
        let mut fs = Self::new(disk);
        if fs.blocks < MIN_BLOCKS {
            return Err(Error::NoSpace);
        }
        // A stale superblock in slot 0 could outrank generation 1.
        fs.disk.write(0, &fs.buf)?;
        let root = Record {
            kind: DIR,
            ..Record::default()
        };
        fs.set_records(&[(ROOT, root)])?;
        fs.commit()?;
        Ok(fs)
    }

    pub fn mount(disk: D) -> Result<Self, Error> {
        let mut fs = Self::new(disk);
        let (newest, older) = match (fs.superblock(0), fs.superblock(1)) {
            (Ok(a), Ok(b)) if a.0 > b.0 => (a, Some(b.2)),
            (Ok(a), Ok(b)) => (b, Some(a.2)),
            (Ok(s), Err(_)) | (Err(_), Ok(s)) => (s, None),
            (Err(e), Err(_)) => return Err(e),
        };
        // The older slot only guards a fallback mount; damage there must not fail this one.
        if let Some(older) = older
            && fs.load_table(older).is_ok()
        {
            fs.table = older;
            fs.newest = fs.reach();
        }
        (fs.generation, fs.blocks, fs.table) = newest;
        fs.load_table(fs.table)?;
        fs.advance();
        Ok(fs)
    }

    pub fn lookup(&mut self, dir: Inode, name: &[u8]) -> Result<Inode, Error> {
        self.scan(dir, |n, _| n == name)?.ok_or(Error::NotFound)
    }

    /// Calls `f` with each entry's name and inode, in creation order.
    pub fn readdir(&mut self, dir: Inode, mut f: impl FnMut(&[u8], Inode)) -> Result<(), Error> {
        self.scan(dir, |n, i| {
            f(n, i);
            false
        })
        .map(|_| ())
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
                    self.load(p)?;
                    out.copy_from_slice(&self.buf[at..at + n]);
                }
            }
            pos += n as u64;
        }
        Ok(end.saturating_sub(offset) as usize)
    }

    /// Writes `data` at `offset`, growing the file; a gap past the old end reads as zeros.
    pub fn write(&mut self, file: Inode, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut r = self.file(file)?;
        self.write_data(&mut r, offset, data)?;
        self.set_records(&[(file, r)])
    }

    /// Makes every change so far durable, atomically.
    pub fn commit(&mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
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
        self.advance();
        Ok(())
    }

    fn new(disk: D) -> Self {
        Self {
            blocks: min(disk.blocks(), MAX_BLOCKS) as u32,
            disk,
            generation: 0,
            table: [0; TABLE_BLOCKS],
            records: [Record::default(); MAX_INODES as usize],
            newest: [0; WORDS],
            // The superblock slots are never free.
            committed: core::array::from_fn(|w| if w == 0 { 0b11 } else { 0 }),
            used: core::array::from_fn(|w| if w == 0 { 0b11 } else { 0 }),
            buf: [0; BLOCK_SIZE],
            cached: None,
            meta: [0; BLOCK_SIZE],
            broken: false,
        }
    }

    /// Generation, block count and inode table of a valid superblock slot.
    fn superblock(&mut self, slot: u32) -> Result<(u64, u32, [u32; TABLE_BLOCKS]), Error> {
        self.load(slot)?;
        let (generation, blocks) = (le64(&self.buf, 8), le32(&self.buf, 16));
        let table = core::array::from_fn(|i| le32(&self.buf, 20 + 4 * i));
        if le64(&self.buf, 0) != MAGIC
            || generation == u64::MAX
            || !(MIN_BLOCKS..=self.blocks).contains(&blocks)
            || !valid(&table, blocks)
        {
            return Err(Error::Corrupt);
        }
        Ok((generation, blocks, table))
    }

    fn load_table(&mut self, table: [u32; TABLE_BLOCKS]) -> Result<(), Error> {
        for (t, b) in table.into_iter().enumerate() {
            if b != 0 {
                self.load(b)?;
            }
            for i in 0..PER_TABLE {
                self.records[t * PER_TABLE + i] = if b == 0 {
                    Record::default()
                } else {
                    self.decode(i)?
                };
            }
        }
        Ok(())
    }

    /// Blocks reachable from the working table.
    fn reach(&self) -> Bitmap {
        let mut map = [0; WORDS];
        map[0] = 0b11;
        let ptrs = self.records.iter().flat_map(|r| &r.ptrs);
        for &b in self.table.iter().chain(ptrs).filter(|&&b| b != 0) {
            map[b as usize / 64] |= 1 << (b % 64);
        }
        map
    }

    /// The working table is now the newest slot's.
    fn advance(&mut self) {
        let reach = self.reach();
        self.committed = core::array::from_fn(|w| reach[w] | self.newest[w]);
        self.newest = reach;
        self.used = self.committed;
    }

    fn free(&self) -> usize {
        self.blocks as usize
            - self
                .used
                .iter()
                .map(|w| w.count_ones() as usize)
                .sum::<usize>()
    }

    fn alloc(&mut self) -> Result<u32, Error> {
        let w = self
            .used
            .iter()
            .position(|&w| w != !0)
            .ok_or(Error::NoSpace)?;
        let b = (w * 64) as u32 + self.used[w].trailing_ones();
        if b >= self.blocks {
            return Err(Error::NoSpace);
        }
        self.used[w] |= 1 << (b % 64);
        Ok(b)
    }

    /// Allocated since the last commit, so no slot reaches it.
    fn fresh(&self, b: u32) -> bool {
        b != 0 && self.committed[b as usize / 64] & (1 << (b % 64)) == 0
    }

    /// Stores the buffer in place of `old`: over it if fresh, else in a new block.
    fn cow(&mut self, old: u32, meta: bool) -> Result<u32, Error> {
        let b = if self.fresh(old) { old } else { self.alloc()? };
        self.store(b, meta)?;
        Ok(b)
    }

    fn load(&mut self, b: u32) -> Result<(), Error> {
        if self.cached == Some(b) {
            return Ok(());
        }
        self.cached = None;
        self.disk.read(b as u64, &mut self.buf)?;
        if le64(&self.buf, PAYLOAD) != checksum(b, &self.buf) {
            return Err(Error::Corrupt);
        }
        self.cached = Some(b);
        Ok(())
    }

    /// Writes `meta` (table and superblock blocks) or `buf` (data blocks, which then stay cached) to block `b`.
    fn store(&mut self, b: u32, meta: bool) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Io);
        }
        let buf = if meta { &mut self.meta } else { &mut self.buf };
        let sum = checksum(b, buf);
        buf[PAYLOAD..].copy_from_slice(&sum.to_le_bytes());
        if let Err(e) = self.disk.write(b as u64, buf) {
            // A fresh block rewritten in place may be torn.
            self.broken = true;
            return Err(e);
        }
        if !meta {
            self.cached = Some(b);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        let r = self.disk.flush();
        self.broken |= r.is_err();
        r
    }

    /// Record `i` of the table block in the buffer.
    fn decode(&self, i: usize) -> Result<Record, Error> {
        let b = &self.buf[i * RECORD..][..RECORD];
        let r = Record {
            kind: b[0],
            size: le32(b, 4),
            ptrs: core::array::from_fn(|p| le32(b, 8 + 4 * p)),
        };
        if r.kind > DIR || r.size as u64 > MAX_FILE_SIZE || !valid(&r.ptrs, self.blocks) {
            return Err(Error::Corrupt);
        }
        Ok(r)
    }

    /// Applies `changes` to the table and writes each table block they touch, after reserving every block needed.
    fn set_records(&mut self, changes: &[(Inode, Record)]) -> Result<(), Error> {
        let t = |k: usize| changes[k].0.0 as usize / PER_TABLE;
        let first = |k: usize| (0..k).all(|j| t(j) != t(k));
        let need = (0..changes.len())
            .filter(|&k| first(k) && !self.fresh(self.table[t(k)]))
            .count();
        if self.free() < need {
            return Err(Error::NoSpace);
        }
        for &(i, r) in changes {
            self.records[i.0 as usize] = r;
        }
        for k in (0..changes.len()).filter(|&k| first(k)) {
            self.write_table(t(k))?;
        }
        Ok(())
    }

    fn write_table(&mut self, t: usize) -> Result<(), Error> {
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
        self.table[t] = self.cow(self.table[t], true)?;
        Ok(())
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

    /// Writes the data blocks of `r` (a copy; the table is untouched) and grows its size.
    fn write_data(&mut self, r: &mut Record, offset: u64, data: &[u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&e| e <= MAX_FILE_SIZE)
            .ok_or(Error::TooBig)?;
        let mut pos = offset;
        while pos < end {
            let (i, at) = (
                (pos / PAYLOAD as u64) as usize,
                (pos % PAYLOAD as u64) as usize,
            );
            let n = min(PAYLOAD - at, (end - pos) as usize);
            if r.ptrs[i] != 0 && n < PAYLOAD {
                self.load(r.ptrs[i])?;
            }
            self.cached = None;
            if r.ptrs[i] == 0 {
                self.buf.fill(0);
            }
            self.buf[at..at + n].copy_from_slice(&data[(pos - offset) as usize..][..n]);
            r.ptrs[i] = self.cow(r.ptrs[i], false)?;
            pos += n as u64;
        }
        r.size = max(r.size, end as u32);
        Ok(())
    }

    /// The first entry of `dir` for which `f` returns true.
    fn scan(
        &mut self,
        dir: Inode,
        mut f: impl FnMut(&[u8], Inode) -> bool,
    ) -> Result<Option<Inode>, Error> {
        let r = self.dir(dir)?;
        for e in 0..r.size as usize / DIRENT {
            let at = e % PER_DIR_BLOCK * DIRENT;
            if at == 0 {
                self.load(r.ptrs[e / PER_DIR_BLOCK])?;
            }
            let d = &self.buf[at..at + DIRENT];
            let (inode, len) = (le32(d, 0), d[4] as usize);
            if inode >= MAX_INODES || len > NAME_MAX {
                return Err(Error::Corrupt);
            }
            if f(&d[5..5 + len], Inode(inode)) {
                return Ok(Some(Inode(inode)));
            }
        }
        Ok(None)
    }

    fn add(&mut self, dir: Inode, name: &[u8], kind: u8) -> Result<Inode, Error> {
        if name.is_empty()
            || name.len() > NAME_MAX
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(Error::InvalidName);
        }
        if self.scan(dir, |n, _| n == name)?.is_some() {
            return Err(Error::Exists);
        }
        let inode = self
            .records
            .iter()
            .position(|r| r.kind == FREE)
            .ok_or(Error::NoSpace)?;
        let inode = Inode(inode as u32);
        let mut entry = [0; DIRENT];
        entry[..4].copy_from_slice(&inode.0.to_le_bytes());
        entry[4] = name.len() as u8;
        entry[5..5 + name.len()].copy_from_slice(name);
        let mut d = self.records[dir.0 as usize];
        let end = d.size as u64;
        self.write_data(&mut d, end, &entry)?;
        let r = Record {
            kind,
            ..Record::default()
        };
        self.set_records(&[(dir, d), (inode, r)])?;
        Ok(inode)
    }
}

/// Every pointer is 0 (none) or a block past the superblocks.
fn valid(ptrs: &[u32], blocks: u32) -> bool {
    ptrs.iter().all(|&p| p == 0 || (2..blocks).contains(&p))
}

/// Four interleaved multiply-rotate lanes over 64-bit words; each step is a bijection, so any one-word change shows.
fn checksum(block: u32, buf: &[u8; BLOCK_SIZE]) -> u64 {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    let (words, _) = buf[..PAYLOAD].as_chunks::<8>();
    let mut lanes = [block as u64, 1, 2, 3];
    for quad in words.chunks(4) {
        for (l, w) in lanes.iter_mut().zip(quad) {
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
