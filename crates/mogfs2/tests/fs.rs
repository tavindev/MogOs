use std::cell::Cell;

use mogfs2::{
    BLOCK_SIZE, Block, Disk, Error, Fs, Inode, Kind, MAX_FILE_SIZE, NAME_MAX, ROOT, bitmap_words,
    cache_blocks,
};

/// In-memory disk with a write-back cache: writes stay pending until a flush makes them durable. After `cut` events
/// (block writes, each block of a request counted, and flushes) every write and flush fails, as if power were lost.
#[derive(Clone)]
struct MemDisk {
    durable: Vec<Block>,
    pending: Vec<(usize, Block)>,
    events: usize,
    cut: usize,
}

impl MemDisk {
    fn new(blocks: usize) -> Self {
        Self {
            durable: vec![[0; BLOCK_SIZE]; blocks],
            pending: Vec::new(),
            events: 0,
            cut: usize::MAX,
        }
    }

    fn with_cut(&self, cut: usize) -> Self {
        Self {
            events: 0,
            cut,
            ..self.clone()
        }
    }

    /// The disk after power loss: the durable blocks plus the pending writes `keep` selects (by index).
    fn crash(&self, keep: impl Fn(usize, usize) -> bool) -> Self {
        let mut disk = Self::new(self.durable.len());
        disk.durable = self.durable.clone();
        for (i, (b, data)) in self.pending.iter().enumerate() {
            if keep(i, *b) {
                disk.durable[*b] = *data;
            }
        }
        disk
    }

    fn event(&mut self) -> Result<(), Error> {
        self.events += 1;
        if self.events > self.cut {
            Err(Error::Io)
        } else {
            Ok(())
        }
    }

    fn block(&mut self, b: usize) -> &mut Block {
        match self.pending.iter_mut().rev().find(|(p, _)| *p == b) {
            Some((_, data)) => data,
            None => &mut self.durable[b],
        }
    }
}

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        for (i, buf) in bufs.iter_mut().enumerate() {
            let b = block as usize + i;
            if b >= self.durable.len() {
                return Err(Error::Io);
            }
            *buf = *self.block(b);
        }
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
        for (i, buf) in bufs.iter().enumerate() {
            let b = block as usize + i;
            if b >= self.durable.len() {
                return Err(Error::Io);
            }
            self.event()?;
            self.pending.push((b, *buf));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.event()?;
        for (b, data) in self.pending.drain(..) {
            self.durable[b] = data;
        }
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.durable.len() as u64
    }
}

/// The memory an `Fs` borrows: its cache (bitmap staging and a node pool) and bitmaps.
struct Mem {
    cache: Vec<Block>,
    bits: Vec<u64>,
}

impl Mem {
    fn new(blocks: usize, pool: usize) -> Self {
        Self {
            cache: vec![[0; BLOCK_SIZE]; cache_blocks(blocks as u64, pool)],
            bits: vec![0; bitmap_words(blocks as u64)],
        }
    }

    fn fs<D: Disk>(&mut self, disk: D) -> Fs<'_, D> {
        Fs::new(disk, &mut self.cache, &mut self.bits)
    }
}

const POOL: usize = 32;

type Tree = Vec<(String, Vec<u8>)>;

/// Every path under `dir` with its contents, sorted; directories end in `/`.
fn walk<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &str, out: &mut Tree) -> Result<(), Error> {
    let mut entries = Vec::new();
    fs.readdir(dir, 0, |name, inode, kind| {
        entries.push((String::from_utf8(name.to_vec()).unwrap(), inode, kind));
        false
    })?;
    for (name, inode, kind) in entries {
        let path = format!("{path}/{name}");
        if kind == Kind::Dir {
            out.push((format!("{path}/"), Vec::new()));
            walk(fs, inode, &path, out)?;
        } else {
            let size = fs.stat(inode)?.size;
            let mut buf = vec![0; size as usize];
            assert_eq!(fs.read(inode, 0, &mut buf)?, size as usize);
            out.push((path, buf));
        }
    }
    Ok(())
}

fn snapshot(disk: &mut MemDisk) -> Result<Tree, Error> {
    let mut mem = Mem::new(disk.durable.len(), POOL);
    let mut fs = mem.fs(disk);
    fs.mount()?;
    let mut out = Vec::new();
    walk(&mut fs, ROOT, "", &mut out)?;
    out.sort();
    Ok(out)
}

fn entry(path: &str, data: &[u8]) -> (String, Vec<u8>) {
    (path.to_string(), data.to_vec())
}

const SEED: u64 = 0x5eed;

#[test]
fn round_trip_survives_remount_and_drops_uncommitted_changes() {
    let mut disk = MemDisk::new(256);
    let mut mem = Mem::new(256, POOL);
    let mut fs = mem.fs(&mut disk);
    fs.format(SEED).unwrap();
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    let big = fs.create(docs, b"big").unwrap();
    fs.write(big, 0, &[1; 9000]).unwrap();
    fs.write(big, 4050, &[2; 100]).unwrap();
    fs.mkdir(ROOT, b"empty").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"lost").unwrap();

    let mut expected = vec![1; 9000];
    expected[4050..4150].fill(2);
    assert_eq!(
        snapshot(&mut disk),
        Ok(vec![
            entry("/docs/", b""),
            entry("/docs/a.txt", b"hello"),
            ("/docs/big".to_string(), expected),
            entry("/empty/", b""),
        ])
    );

    let mut fs = mem.fs(&mut disk);
    fs.mount().unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    let a = fs.lookup(docs, b"a.txt").unwrap();
    assert_eq!(fs.kind(docs), Ok(Kind::Dir));
    assert_eq!(fs.kind(a), Ok(Kind::File));
    let mut buf = [0; 8];
    assert_eq!(fs.read(a, 1, &mut buf), Ok(4));
    assert_eq!(&buf[..4], b"ello");
    assert_eq!(fs.read(a, 9, &mut buf), Ok(0));
    // Mounting again in place starts over from the disk.
    fs.create(ROOT, b"gone").unwrap();
    fs.mount().unwrap();
    assert_eq!(fs.lookup(ROOT, b"gone"), Err(Error::NotFound));
}
