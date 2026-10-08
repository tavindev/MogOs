use std::cell::Cell;
use std::slice::from_mut;

use mogfs::{
    BLOCK_SIZE, Buf, Disk, Error, Fs, Inode, Kind, MAX_FILE_SIZE, NAME_MAX, Page, ROOT,
    bitmap_words, cache_blocks,
};

/// In-memory disk with a write-back cache: writes stay pending until a flush makes them durable. After `cut` events
/// (block writes, each block of a request counted, and flushes) every write and flush fails, as if power were lost.
#[derive(Clone)]
struct MemDisk {
    durable: Vec<Buf>,
    pending: Vec<(usize, Buf)>,
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

    fn block(&mut self, b: usize) -> &mut Buf {
        match self.pending.iter_mut().rev().find(|(p, _)| *p == b) {
            Some((_, data)) => data,
            None => &mut self.durable[b],
        }
    }
}

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        for (i, buf) in bufs.iter_mut().enumerate() {
            let b = block as usize + i;
            if b >= self.durable.len() {
                return Err(Error::Io);
            }
            *buf = *self.block(b);
        }
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
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
    cache: Vec<Buf>,
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
/// Superblock bytes before the live page list.
const SB_HDR: usize = 120;

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

fn format<'a>(mem: &'a mut Mem, disk: &'a mut MemDisk) -> Fs<'a, &'a mut MemDisk> {
    let mut fs = mem.fs(disk);
    fs.format(SEED).unwrap();
    fs
}

fn mount<'a>(mem: &'a mut Mem, disk: &'a mut MemDisk) -> Result<Fs<'a, &'a mut MemDisk>, Error> {
    let mut fs = mem.fs(disk);
    fs.mount()?;
    Ok(fs)
}

/// Formats and commits `/docs/a.txt` = "hello": generation 1 (empty) in slot 1, generation 2 in slot 0.
fn hello() -> MemDisk {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    fs.commit().unwrap();
    disk
}

fn hello_tree() -> Tree {
    vec![entry("/docs/", b""), entry("/docs/a.txt", b"hello")]
}

fn names<D: Disk>(fs: &mut Fs<D>, dir: Inode) -> Vec<String> {
    let mut out = Vec::new();
    fs.readdir(dir, 0, |n, _, _| {
        out.push(String::from_utf8(n.to_vec()).unwrap());
        false
    })
    .unwrap();
    out.sort();
    out
}

#[test]
fn the_disk_can_be_replaced_before_mount() {
    let (mut a, mut b) = (hello(), MemDisk::new(64));
    let mut mem = Mem::new(64, POOL);
    let mut fs = mem.fs(&mut b);
    *fs.disk() = &mut a;
    fs.mount().unwrap();
    assert!(fs.lookup(ROOT, b"docs").is_ok());
}

#[test]
fn truncate_empties_a_file_and_frees_its_blocks() {
    let mut disk = MemDisk::new(96);
    let mut mem = Mem::new(96, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[1; 9000]).unwrap();
    fs.commit().unwrap();
    // Blocks written since the last commit are free again at once.
    for _ in 0..100 {
        fs.truncate(f).unwrap();
        fs.write(f, 0, &[2; 9000]).unwrap();
    }
    fs.truncate(f).unwrap();
    assert_eq!(fs.read(f, 0, &mut [0; 1]), Ok(0));
    fs.write(f, 0, b"x").unwrap();
    assert_eq!(fs.truncate(ROOT), Err(Error::IsDir));
    fs.commit().unwrap();
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/f", b"x")]));
}

#[test]
fn unlink_removes_files_and_empty_directories_and_never_reuses_inodes() {
    let mut disk = MemDisk::new(96);
    let mut mem = Mem::new(96, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let a = fs.create(ROOT, b"a").unwrap();
    fs.write(a, 0, &[1; 9000]).unwrap();
    fs.create(ROOT, b"b").unwrap();
    let d = fs.mkdir(ROOT, b"d").unwrap();
    fs.create(d, b"x").unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.unlink(ROOT, b"d"), Err(Error::NotEmpty));
    assert_eq!(fs.unlink(ROOT, b"missing"), Err(Error::NotFound));
    assert_eq!(fs.unlink(ROOT, b".."), Err(Error::InvalidName));
    assert_eq!(fs.unlink(a, b"x"), Err(Error::NotDir));
    fs.unlink(ROOT, b"a").unwrap();
    assert_eq!(names(&mut fs, ROOT), ["b", "d"]);
    assert_eq!(fs.kind(a), Err(Error::NotFound));
    fs.unlink(d, b"x").unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    // Freed blocks of committed and uncommitted files come back; inode numbers do not.
    let mut seen = vec![a, d];
    for _ in 0..100 {
        let f = fs.create(ROOT, b"f").unwrap();
        assert!(!seen.contains(&f));
        seen.push(f);
        fs.write(f, 0, &[2; 9000]).unwrap();
        fs.commit().unwrap();
        fs.unlink(ROOT, b"f").unwrap();
    }
    fs.commit().unwrap();
    assert_eq!(fs.read(a, 0, &mut [0; 1]), Err(Error::NotFound));
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/b", b"")]));
}

#[test]
fn rename_moves_entries_within_and_across_directories() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let a = fs.mkdir(ROOT, b"a").unwrap();
    let b = fs.mkdir(a, b"b").unwrap();
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, b"data").unwrap();
    fs.create(ROOT, b"g").unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.rename(ROOT, b"f", ROOT, b"g"), Err(Error::Exists));
    assert_eq!(fs.rename(ROOT, b"g", ROOT, b"f"), Err(Error::Exists));
    assert_eq!(fs.rename(ROOT, b"no", ROOT, b"h"), Err(Error::NotFound));
    assert_eq!(fs.rename(ROOT, b"no", ROOT, b"f"), Err(Error::NotFound));
    // Renaming an entry to itself does nothing.
    fs.rename(ROOT, b"f", ROOT, b"f").unwrap();
    assert_eq!(fs.rename(ROOT, b"no", ROOT, b"no"), Err(Error::NotFound));
    assert_eq!(fs.rename(ROOT, b"f", f, b"h"), Err(Error::NotDir));
    assert_eq!(fs.rename(ROOT, b"f", ROOT, b"/"), Err(Error::InvalidName));
    // A directory cannot move into itself or below itself.
    assert_eq!(fs.rename(ROOT, b"a", a, b"x"), Err(Error::InvalidName));
    assert_eq!(fs.rename(ROOT, b"a", b, b"x"), Err(Error::InvalidName));
    fs.rename(ROOT, b"f", ROOT, b"h").unwrap();
    fs.rename(ROOT, b"h", b, b"f").unwrap();
    fs.rename(a, b"b", ROOT, b"b").unwrap();
    assert_eq!(fs.lookup(ROOT, b"b"), Ok(b));
    // `b` left `a`, so `a` may now move below it.
    fs.rename(ROOT, b"a", b, b"a").unwrap();
    fs.commit().unwrap();
    assert_eq!(
        snapshot(&mut disk),
        Ok(vec![
            entry("/b/", b""),
            entry("/b/a/", b""),
            entry("/b/f", b"data"),
            entry("/g", b""),
        ])
    );
}

#[test]
fn stat_reports_kind_size_mode_links_and_times() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = mem.fs(&mut disk);
    let t = |s: u64| s * 1_000_000_000_000_000_000;
    fs.set_time(1);
    fs.format(SEED).unwrap();
    fs.set_time(t(5));
    let d = fs.mkdir(ROOT, b"d").unwrap();
    fs.set_time(t(6));
    let f = fs.create(d, b"f").unwrap();
    fs.set_time(t(7));
    fs.write(f, 10, b"x").unwrap();
    fs.commit().unwrap();
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let stat = |kind, size, mode, mtime, ctime, btime| mogfs::Stat {
        kind,
        size,
        mode,
        links: 1,
        mtime,
        ctime,
        btime,
    };
    assert_eq!(
        fs.stat(f),
        Ok(stat(Kind::File, 11, 0o644, t(7), t(7), t(6)))
    );
    assert_eq!(fs.stat(d), Ok(stat(Kind::Dir, 0, 0o755, t(6), t(6), t(5))));
    assert_eq!(fs.stat(ROOT), Ok(stat(Kind::Dir, 0, 0o755, t(5), t(5), 1)));
}

/// The format's hash, restated.
fn sum(block: u64, buf: &[u8]) -> u64 {
    let mut lanes: [u64; 16] = std::array::from_fn(|i| i as u64);
    lanes[0] ^= block | 1 << 63;
    for (i, w) in buf.chunks(8).enumerate() {
        lanes[i % 16] = mix(lanes[i % 16], u64::from_le_bytes(w.try_into().unwrap()));
    }
    lanes.into_iter().fold(0, mix)
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// `disk` with superblock field `i` of each slot in `slots` set to `value`, resealed.
fn crafted(mut disk: MemDisk, slots: &[usize], i: usize, value: u64) -> MemDisk {
    for &s in slots {
        let sb = &mut disk.durable[s];
        sb[8 * i..8 * i + 8].copy_from_slice(&value.to_le_bytes());
        let sum = sum(s as u64, &sb[..BLOCK_SIZE - 8]);
        sb[BLOCK_SIZE - 8..].copy_from_slice(&sum.to_le_bytes());
    }
    disk
}

#[test]
fn map_gives_each_page_block_and_sum_for_verify() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[1; 5000]).unwrap();
    fs.write(f, 3 * BLOCK_SIZE as u64, b"end").unwrap();
    assert_eq!(fs.map(f, Page(2)), Ok(None));
    assert_eq!(fs.map(ROOT, Page(0)), Err(Error::IsDir));
    let pages: Vec<_> = [0, 1, 3]
        .map(|p| fs.map(f, Page(p)).unwrap().unwrap())
        .to_vec();
    fs.commit().unwrap();
    for (i, (block, sum)) in pages.into_iter().enumerate() {
        let page = disk.durable[block.0 as usize];
        assert_eq!(mogfs::verify(block, &page, sum), Ok(()));
        assert_eq!(page[0], if i < 2 { 1 } else { b'e' });
        assert_eq!(mogfs::verify(block + 1, &page, sum), Err(Error::Corrupt));
    }
}

#[test]
fn map_writes_a_page_still_in_the_buffer() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, b"buffered").unwrap();
    let (block, sum) = fs.map(f, Page(0)).unwrap().unwrap();
    let mut page = [0; BLOCK_SIZE];
    fs.disk().read(block.0, from_mut(&mut page)).unwrap();
    assert_eq!(mogfs::verify(block, &page, sum), Ok(()));
    assert_eq!(&page[..8], b"buffered");
}

#[test]
fn flipped_byte_in_any_block_reads_as_corrupt_or_falls_back() {
    let disk = hello();
    let (mut corrupt, mut fallback) = (0, 0);
    for block in 0..disk.durable.len() {
        for at in [100, BLOCK_SIZE - 1] {
            let mut bad = disk.clone();
            bad.durable[block][at] ^= 1;
            match snapshot(&mut bad) {
                Err(Error::Corrupt) => corrupt += 1,
                Ok(t) if t.is_empty() => fallback += 1,
                other => assert_eq!(other, Ok(hello_tree()), "block {block} byte {at}"),
            }
        }
    }
    // The data, and the bitmap page both slots share (the newest only logs its changes), read as corrupt. The newest
    // superblock and tree (one leaf, on the rightmost path mount checks) fall back to the empty older slot; flips in
    // the older slot change nothing.
    assert_eq!((corrupt, fallback), (4, 4));
}

#[test]
fn mount_falls_back_to_the_older_superblock() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    fs.create(ROOT, b"b").unwrap();
    fs.commit().unwrap();
    // Generation 3 is in slot 1; generation 2 (hello) in slot 0.
    let mut bad = disk.clone();
    bad.durable[1][BLOCK_SIZE - 1] ^= 1;
    assert_eq!(snapshot(&mut bad), Ok(hello_tree()));
    bad.durable[0][0] ^= 1;
    assert_eq!(snapshot(&mut bad), Err(Error::Corrupt));
}

#[test]
fn uncommitted_changes_leave_the_older_slot_intact() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    fs.create(docs, b"b").unwrap();
    fs.mkdir(ROOT, b"c").unwrap();
    // Slot 0 holds hello; slot 1 the empty file system from `format`.
    let mut bad = disk.clone();
    bad.durable[0][0] ^= 1;
    assert_eq!(snapshot(&mut bad), Ok(vec![]));

    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    fs.create(docs, b"b").unwrap();
    fs.mkdir(ROOT, b"c").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"d").unwrap();
    fs.commit().unwrap();
    // Generation 4 is in slot 0; generation 3, whose blocks stayed reserved through 4's changes, in slot 1.
    disk.durable[0][0] ^= 1;
    let mut tree = hello_tree();
    tree.push(entry("/c/", b""));
    tree.push(entry("/docs/b", b""));
    tree.sort();
    assert_eq!(snapshot(&mut disk), Ok(tree));
}

#[test]
fn crafted_superblocks_are_rejected() {
    // A block count too small or past the disk; a root table of two roots; a tree too tall; a root at a superblock; a
    // log past the superblock's room, or one whose words are out of order or past the disk; a wrong index height.
    for (i, value) in [
        (2, 0),
        (2, 15),
        (2, 65),
        (8, 2),
        (12, 8),
        (9, 1),
        (13, 248),
        (13, 2),
        (14, 1),
        (17, 1),
    ] {
        let mut disk = crafted(hello(), &[0], i, value);
        assert_eq!(snapshot(&mut disk), Ok(vec![]), "field {i} = {value}");
    }
    // A generation in the wrong slot: slot 0 is skipped, slot 1 (generation 1, empty) is used.
    let mut disk = crafted(hello(), &[0], 1, 3);
    assert_eq!(snapshot(&mut disk), Ok(vec![]));
    assert_eq!(
        Fs::new(&mut MemDisk::new(8), &mut [], &mut []).format(SEED),
        Err(Error::TooBig)
    );
    let mut mem = Mem::new(8, POOL);
    assert_eq!(
        mem.fs(&mut MemDisk::new(8)).format(SEED),
        Err(Error::NoSpace)
    );
    // Memory too small for the disk.
    let mut disk = hello();
    let mut small = Mem::new(64, POOL);
    small.bits.clear();
    assert_eq!(small.fs(&mut disk).mount(), Err(Error::TooBig));
}

#[test]
fn an_unknown_incompatible_feature_refuses_the_mount() {
    let mut disk = crafted(hello(), &[0], 3, 1 << 7);
    assert_eq!(snapshot(&mut disk), Err(Error::Unsupported));
    // On the older slot only, it is not needed.
    let mut disk = crafted(hello(), &[1], 3, 1);
    assert_eq!(snapshot(&mut disk), Ok(hello_tree()));
}

#[test]
fn the_inode_counter_must_exceed_every_inode() {
    // Inodes 0, 1 (`docs`) and 2 (`a.txt`): a counter of 2 could hand out `a.txt`'s number again.
    let mut disk = crafted(hello(), &[0], 4, 2);
    assert_eq!(snapshot(&mut disk), Ok(vec![]));
    let mut disk = crafted(hello(), &[0], 4, 3);
    assert_eq!(snapshot(&mut disk), Ok(hello_tree()));
}

#[test]
fn the_last_generation_commits_and_then_commits_fail() {
    let mut disk = crafted(hello(), &[0], 1, u64::MAX - 1);
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    fs.create(ROOT, b"new").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"lost").unwrap();
    assert_eq!(fs.commit(), Err(Error::Corrupt));
    let mut tree = hello_tree();
    tree.push(entry("/new", b""));
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(tree));
}

/// Fails the next `.2` reads of block `.1`.
struct FailReads<'a>(&'a mut MemDisk, u64, usize);

impl Disk for FailReads<'_> {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        if block == self.1 && self.2 > 0 {
            self.2 -= 1;
            return Err(Error::Io);
        }
        (&mut *self.0).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        (&mut *self.0).write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        (&mut *self.0).flush()
    }

    fn blocks(&self) -> u64 {
        self.0.durable.len() as u64
    }
}

#[test]
fn io_error_at_mount_is_not_a_fallback() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mem.fs(FailReads(&mut disk, 0, 1));
    assert_eq!(fs.mount(), Err(Error::Io));
    fs.mount().unwrap();
    fs.create(ROOT, b"new").unwrap();
    fs.commit().unwrap();
    let mut tree = hello_tree();
    tree.push(entry("/new", b""));
    assert_eq!(snapshot(&mut disk), Ok(tree));
}

#[test]
fn failed_mount_leaves_the_fs_read_only() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    let a = fs.lookup(docs, b"a.txt").unwrap();
    // The first request, for the superblocks, fails.
    let mut fs = mem.fs(FailReads(&mut disk, 0, usize::MAX));
    assert_eq!(fs.mount(), Err(Error::Io));
    assert_eq!(fs.write(a, 0, b"HE"), Err(Error::Io));
    assert_eq!(fs.truncate(a), Err(Error::Io));
    assert_eq!(fs.unlink(docs, b"a.txt"), Err(Error::Io));
    assert_eq!(fs.rename(docs, b"a.txt", ROOT, b"a.txt"), Err(Error::Io));
    assert_eq!(fs.create(docs, b"new"), Err(Error::Io));
    assert_eq!(fs.commit(), Err(Error::Io));
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(hello_tree()));
}

/// Mounts `disk`, overwrites `/docs/a.txt`, adds `/src` and `/docs/b.txt`, renames `b.txt` to `c.txt`, moves `a.txt`
/// and then `docs` into `/src`, and commits.
fn change(disk: &mut MemDisk) -> Result<(), Error> {
    let mut mem = Mem::new(disk.durable.len(), POOL);
    let mut fs = mount(&mut mem, disk)?;
    let docs = fs.lookup(ROOT, b"docs")?;
    let a = fs.lookup(docs, b"a.txt")?;
    fs.write(a, 0, b"HE")?;
    let src = fs.mkdir(ROOT, b"src")?;
    let b = fs.create(docs, b"b.txt")?;
    fs.write(b, 0, &[7; 9000])?;
    fs.rename(docs, b"b.txt", docs, b"c.txt")?;
    fs.rename(docs, b"a.txt", src, b"a.txt")?;
    fs.rename(ROOT, b"docs", src, b"docs")?;
    fs.commit()
}

/// Mounts `disk` after `change` and unlinks everything under `/src`, then commits.
fn remove(disk: &mut MemDisk) -> Result<(), Error> {
    let mut mem = Mem::new(disk.durable.len(), POOL);
    let mut fs = mount(&mut mem, disk)?;
    let src = fs.lookup(ROOT, b"src")?;
    let docs = fs.lookup(src, b"docs")?;
    fs.unlink(src, b"a.txt")?;
    fs.unlink(docs, b"c.txt")?;
    fs.unlink(src, b"docs")?;
    fs.commit()
}

/// Mounts `disk` and adds 60 files with long names and a page each: leaves split, and dirty nodes are written out
/// before the commit.
fn grow(disk: &mut MemDisk) -> Result<(), Error> {
    let mut mem = Mem::new(disk.durable.len(), POOL);
    let mut fs = mount(&mut mem, disk)?;
    let d = fs.mkdir(ROOT, b"grow")?;
    for i in 0..60 {
        let f = fs.create(d, format!("{i:0>200}").as_bytes())?;
        fs.write(f, 0, &[i as u8; 10])?;
    }
    fs.commit()
}

#[test]
fn power_cut_anywhere_leaves_the_old_or_the_new_state() {
    let mut changed = hello();
    change(&mut changed).unwrap();
    power_cut(hello(), change);
    power_cut(changed, remove);
    let mut big = MemDisk::new(256);
    let mut mem = Mem::new(256, POOL);
    format(&mut mem, &mut big);
    power_cut(big, grow);
}

fn power_cut(base: MemDisk, change: fn(&mut MemDisk) -> Result<(), Error>) {
    let old = snapshot(&mut base.clone()).unwrap();
    let mut full = base.with_cut(usize::MAX);
    change(&mut full).unwrap();
    let new = snapshot(&mut full).unwrap();
    assert_ne!(old, new);

    for cut in 0..=full.events {
        let mut disk = base.with_cut(cut);
        assert_eq!(change(&mut disk).is_ok(), cut == full.events, "cut {cut}");
        let n = disk.pending.len();
        let mut lands: Vec<Box<dyn Fn(usize, usize) -> bool>> = vec![
            Box::new(|_, _| false),
            Box::new(|_, _| true),
            Box::new(|_, b| b < 2),
            Box::new(|i, _| i % 2 == 0),
        ];
        for skip in 0..n {
            lands.push(Box::new(move |i, _| i != skip));
        }
        for keep in &lands {
            let state = snapshot(&mut disk.crash(keep)).unwrap();
            assert!(state == old || state == new, "cut {cut}: {state:?}");
        }
        if cut == full.events {
            assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(new.clone()));
        }
    }
}

#[test]
fn io_from_commit_means_unknown() {
    let base = hello();
    let mut full = base.with_cut(usize::MAX);
    change(&mut full).unwrap();
    // Every write lands and only the final flush fails: the superblock may or may not be durable.
    let mut disk = base.with_cut(full.events - 1);
    assert_eq!(change(&mut disk), Err(Error::Io));
    let old = snapshot(&mut base.clone()).unwrap();
    let new = snapshot(&mut full).unwrap();
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(new));
    assert_eq!(snapshot(&mut disk.crash(|_, _| false)), Ok(old));
}

#[test]
fn commits_reclaim_replaced_blocks() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    for i in 0..1000 {
        fs.write(f, 0, &[i as u8; 9000]).unwrap();
        fs.commit().unwrap();
    }
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/f", &[231; 9000])]));
}

#[test]
fn mount_needs_no_zeroed_memory() {
    // Pages a fresh file takes until `NoSpace`, on a just-formatted disk (its older slot empty).
    let fill = |mem: &mut Mem| {
        let mut disk = MemDisk::new(64);
        format(&mut Mem::new(64, POOL), &mut disk);
        let mut fs = mount(mem, &mut disk).unwrap();
        let f = fs.create(ROOT, b"f").unwrap();
        (0..)
            .take_while(|&p| fs.write(f, p * BLOCK_SIZE as u64, &[1; BLOCK_SIZE]).is_ok())
            .count()
    };
    let mut dirty = Mem::new(64, POOL);
    dirty.cache.iter_mut().for_each(|b| b.fill(0xa5));
    dirty.bits.fill(u64::MAX);
    assert_eq!(fill(&mut dirty), fill(&mut Mem::new(64, POOL)));
}

#[test]
fn limits_and_misuse_return_errors() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let long = [b'x'; NAME_MAX + 1];
    for bad in [&long[..], b"", b".", b"..", b"a/b", b"a\0"] {
        assert_eq!(fs.create(ROOT, bad), Err(Error::InvalidName));
    }
    let f = fs.create(ROOT, &long[..NAME_MAX]).unwrap();
    assert_eq!(fs.lookup(ROOT, &long[..NAME_MAX]), Ok(f));
    assert_eq!(fs.mkdir(ROOT, &long[..NAME_MAX]), Err(Error::Exists));
    // `create` opens what is already there, a directory too.
    assert_eq!(fs.create(ROOT, &long[..NAME_MAX]), Ok(f));
    let d = fs.mkdir(ROOT, b"d").unwrap();
    assert_eq!(fs.create(ROOT, b"d"), Ok(d));
    assert_eq!(fs.kind(d), Ok(Kind::Dir));
    assert_eq!(fs.lookup(ROOT, b"missing"), Err(Error::NotFound));
    assert_eq!(fs.lookup(ROOT, b".."), Err(Error::InvalidName));
    assert_eq!(fs.lookup(f, b"x"), Err(Error::NotDir));
    assert_eq!(fs.create(f, b"x"), Err(Error::NotDir));
    assert_eq!(fs.read(ROOT, 0, &mut [0; 1]), Err(Error::IsDir));
    assert_eq!(fs.write(ROOT, 0, b"x"), Err(Error::IsDir));

    assert_eq!(fs.write(f, MAX_FILE_SIZE - 1, b"ab"), Err(Error::TooBig));
    fs.write(f, MAX_FILE_SIZE - 1, b"a").unwrap();
    let mut buf = [9; 2];
    assert_eq!(fs.read(f, 0, &mut buf), Ok(2));
    assert_eq!(buf, [0, 0]);
    assert_eq!(fs.read(f, MAX_FILE_SIZE - 1, &mut buf), Ok(1));
    assert_eq!(buf[0], b'a');
    fs.commit().unwrap();
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let long = String::from_utf8(long[..NAME_MAX].to_vec()).unwrap();
    assert_eq!(names(&mut fs, ROOT), ["d".to_string(), long]);
}

#[test]
fn no_space_changes_nothing() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[7; 4096]).unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.write(f, 0, &[1; 64 * 4096]), Err(Error::NoSpace));
    let mut b = [0; 1];
    fs.read(f, 0, &mut b).unwrap();
    assert_eq!(b[0], 7);
    // Fill the disk a page at a time; the write that does not fit changes nothing, and the commit still fits.
    let mut at = 4096;
    while fs.write(f, at, &[2; 4096]).is_ok() {
        at += 4096;
    }
    assert_eq!(fs.stat(f).unwrap().size, at);
    fs.commit().unwrap();
    let mut expected = vec![2; at as usize];
    expected[..4096].fill(7);
    assert_eq!(snapshot(&mut disk), Ok(vec![("/f".to_string(), expected)]));
}

/// The name hash's step, restated, so tests can build colliding names from the seed.
fn mix(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29)
}

/// Sixteen-byte names that all hash alike under `seed`: the second word cancels the first's difference.
fn colliding(seed: u64, n: usize) -> Vec<[u8; 16]> {
    let h0 = seed ^ 16;
    let a = u64::from_le_bytes(*b"aaaaaaaa");
    let target = mix(h0, a) ^ u64::from_le_bytes(*b"collides");
    let mut out = vec![];
    for w1 in a.. {
        let w2 = mix(h0, w1) ^ target;
        let mut name = [0; 16];
        name[..8].copy_from_slice(&w1.to_le_bytes());
        name[8..].copy_from_slice(&w2.to_le_bytes());
        if !name.contains(&0) && !name.contains(&b'/') {
            out.push(name);
            if out.len() == n {
                break;
            }
        }
    }
    out
}

/// The inverse of an odd multiplier mod 2^64 (Newton's iteration).
fn inverse(c: u64) -> u64 {
    (0..6).fold(c, |x, _| {
        x.wrapping_mul(2u64.wrapping_sub(c.wrapping_mul(x)))
    })
}

/// The inverse of `h ^= h >> s`.
fn unshift(y: u64, s: u32) -> u64 {
    (0..64 / s).fold(y, |x, _| y ^ x >> s)
}

/// A sixteen-byte name whose hash chain under `seed` is the last one, its end at the largest offset.
fn last_chain_name(seed: u64) -> [u8; 16] {
    let mut h = unshift(u64::MAX, 29).wrapping_mul(inverse(0xbf58_476d_1ce4_e5b9));
    h = unshift(h, 31);
    let before = h
        .rotate_right(29)
        .wrapping_mul(inverse(0x9e37_79b9_7f4a_7c15));
    let h0 = seed ^ 16;
    for w1 in u64::from_le_bytes(*b"aaaaaaaa").. {
        let w2 = before ^ mix(h0, w1);
        let mut name = [0; 16];
        name[..8].copy_from_slice(&w1.to_le_bytes());
        name[8..].copy_from_slice(&w2.to_le_bytes());
        if !name.contains(&0) && !name.contains(&b'/') {
            return name;
        }
    }
    unreachable!()
}

#[test]
fn the_last_hash_chain_holds_its_names() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let name = last_chain_name(SEED);
    let f = fs.create(ROOT, &name).unwrap();
    assert_eq!(fs.lookup(ROOT, &name), Ok(f));
    assert_eq!(fs.create(ROOT, &name), Ok(f));
    fs.commit().unwrap();
    fs.unlink(ROOT, &name).unwrap();
    assert_eq!(fs.lookup(ROOT, &name), Err(Error::NotFound));
}

#[test]
fn colliding_names_fill_a_bounded_chain() {
    let mut disk = MemDisk::new(64);
    let mut mem = Mem::new(64, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let names = colliding(SEED, 9);
    let files: Vec<_> = names[..8]
        .iter()
        .map(|n| fs.create(ROOT, n).unwrap())
        .collect();
    assert_eq!(fs.create(ROOT, &names[8]), Err(Error::Collision));
    assert_eq!(fs.mkdir(ROOT, &names[8]), Err(Error::Collision));
    assert_eq!(fs.lookup(ROOT, &names[8]), Err(Error::NotFound));
    fs.create(ROOT, b"other").unwrap();
    assert_eq!(
        fs.rename(ROOT, b"other", ROOT, &names[8]),
        Err(Error::Collision)
    );
    for (n, f) in names.iter().zip(&files) {
        assert_eq!(fs.lookup(ROOT, n), Ok(*f));
    }
    // Freeing a slot in the middle of the chain makes room.
    fs.unlink(ROOT, &names[3]).unwrap();
    assert_eq!(fs.lookup(ROOT, &names[4]), Ok(files[4]));
    let ninth = fs.create(ROOT, &names[8]).unwrap();
    fs.commit().unwrap();
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    assert_eq!(fs.lookup(ROOT, &names[8]), Ok(ninth));
    assert_eq!(fs.lookup(ROOT, &names[3]), Err(Error::NotFound));
    let mut listed = 0;
    let end = fs
        .readdir(ROOT, 0, |_, _, _| {
            listed += 1;
            false
        })
        .unwrap();
    assert_eq!((listed, end), (9, u64::MAX));
}

#[test]
fn readdir_resumes_from_its_cursor_without_skipping_or_repeating() {
    let mut disk = MemDisk::new(256);
    let mut mem = Mem::new(256, POOL);
    let mut fs = format(&mut mem, &mut disk);
    for i in 0..300 {
        fs.create(ROOT, format!("{i:0>40}").as_bytes()).unwrap();
    }
    // Ten entries a call; between calls, unlink one entry already listed and one not yet listed.
    let (mut cursor, mut listed, mut gone_early) = (0, vec![], vec![]);
    while cursor != u64::MAX {
        let mut n = 0;
        let mut stopped = None;
        cursor = fs
            .readdir(ROOT, cursor, |name, _, _| {
                if n == 10 {
                    stopped = Some(name.to_vec());
                    return true;
                }
                n += 1;
                listed.push(name.to_vec());
                false
            })
            .unwrap();
        // The entry it stopped at comes first in the next call.
        if let Some(s) = stopped.clone() {
            let mut first = None;
            fs.readdir(ROOT, cursor, |name, _, _| {
                first = Some(name.to_vec());
                true
            })
            .unwrap();
            assert_eq!(first, Some(s));
        }
        let all: Vec<Vec<u8>> = names(&mut fs, ROOT)
            .into_iter()
            .map(String::into_bytes)
            .collect();
        if let Some(a) = all.iter().find(|a| listed.contains(a)) {
            fs.unlink(ROOT, a).unwrap();
        }
        if let Some(b) = all
            .iter()
            .find(|b| !listed.contains(b) && Some(*b) != stopped.as_ref())
        {
            fs.unlink(ROOT, b).unwrap();
            gone_early.push(b.clone());
        }
    }
    let mut unique = listed.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), listed.len(), "an entry repeated");
    // Every entry is listed unless it was unlinked before its turn.
    assert_eq!(listed.len() + gone_early.len(), 300);
    assert_eq!(fs.readdir(ROOT, u64::MAX, |_, _, _| true), Ok(u64::MAX));
}

/// `disk` with slot 0's root leaf changed by `f`, resealed up to the superblock.
fn crafted_leaf(mut disk: MemDisk, f: impl FnOnce(&mut Buf)) -> MemDisk {
    let b = le64(&disk.durable[0], 9 * 8) as usize;
    f(&mut disk.durable[b]);
    let s = sum(b as u64, &disk.durable[b][..BLOCK_SIZE - 8]);
    disk.durable[b][BLOCK_SIZE - 8..].copy_from_slice(&s.to_le_bytes());
    crafted(disk, &[0], 10, s)
}

/// Where `name` starts in `block` (a directory entry's name; its inode is the 8 bytes 9 before).
fn find(block: &Buf, name: &[u8]) -> usize {
    block.windows(name.len()).position(|w| w == name).unwrap()
}

#[test]
fn crafted_entries_naming_the_root_or_their_directory_are_corrupt() {
    for child in [0u64, 1] {
        let disk = crafted_leaf(hello(), |leaf| {
            let at = find(leaf, b"a.txt") - 9;
            leaf[at..at + 8].copy_from_slice(&child.to_le_bytes());
        });
        // The only leaf is on the rightmost path, so mount falls back to the empty older slot.
        assert_eq!(snapshot(&mut disk.clone()), Ok(vec![]), "child {child}");
    }
}

#[test]
fn an_inode_is_reached_only_through_the_entry_it_records() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    fs.mkdir(ROOT, b"elsewhere").unwrap();
    fs.commit().unwrap();
    fs.mkdir(ROOT, b"y").unwrap();
    fs.commit().unwrap();
    // `elsewhere` now names `docs` (inode 1) too: a second way to reach it.
    let mut bad = crafted_leaf(disk, |leaf| {
        let at = find(leaf, b"elsewhere") - 9;
        leaf[at..at + 8].copy_from_slice(&1u64.to_le_bytes());
    });
    let mut fs = mount(&mut mem, &mut bad).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    assert_eq!(fs.lookup(ROOT, b"elsewhere"), Err(Error::Corrupt));
    assert_eq!(fs.unlink(ROOT, b"elsewhere"), Err(Error::Corrupt));
    assert_eq!(
        fs.rename(ROOT, b"elsewhere", docs, b"x"),
        Err(Error::Corrupt)
    );
    assert_eq!(fs.create(ROOT, b"elsewhere"), Err(Error::Corrupt));
    assert!(fs.lookup(docs, b"a.txt").is_ok());
}

#[test]
fn the_last_generation_is_not_passed_when_nodes_are_written_out() {
    // Slot 1 (the empty file system) claims the last generation; enough changes to write nodes out before a commit.
    let mut disk = MemDisk::new(512);
    let mut mem = Mem::new(512, POOL);
    let mut fs = format(&mut mem, &mut disk);
    fs.create(ROOT, b"a").unwrap();
    fs.commit().unwrap();
    let mut disk = crafted(disk, &[1], 1, u64::MAX);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let r = (0..300).try_for_each(|i| fs.create(ROOT, format!("{i:0>100}").as_bytes()).map(|_| ()));
    assert_eq!(r, Err(Error::Corrupt));
    assert_eq!(fs.commit(), Err(Error::Io));
}

#[test]
fn pages_past_the_largest_file_map_to_nothing() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    let a = fs
        .lookup(ROOT, b"docs")
        .and_then(|d| fs.lookup(d, b"a.txt"))
        .unwrap();
    assert!(fs.map(a, Page(0)).unwrap().is_some());
    for page in [1 << 40, 1 << 62, 1 << 63, u64::MAX] {
        assert_eq!(fs.map(a, Page(page)), Ok(None), "page {page}");
    }
}

/// Counts the read, write and flush requests that reach `disk`.
struct Counted<'a>(&'a mut MemDisk, &'a Cell<[usize; 3]>);

impl Counted<'_> {
    fn bump(&self, i: usize) {
        let mut c = self.1.get();
        c[i] += 1;
        self.1.set(c);
    }
}

impl Disk for Counted<'_> {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        self.bump(0);
        (&mut *self.0).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        self.bump(1);
        (&mut *self.0).write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.bump(2);
        (&mut *self.0).flush()
    }

    fn blocks(&self) -> u64 {
        self.0.durable.len() as u64
    }
}

#[test]
fn an_older_slot_sharing_a_page_block_needs_its_sum() {
    // Both slots list the same bitmap page; the older slot's sum for it, changed and resealed, makes that slot
    // invalid, so its blocks are not held back from the live tree.
    let disk = hello();
    let sum_at = SB_HDR + 8;
    let good = le64(&disk.durable[1], sum_at);
    assert_eq!(
        le64(&disk.durable[1], SB_HDR),
        le64(&disk.durable[0], SB_HDR)
    );
    let bad = crafted(disk.clone(), &[1], sum_at / 8, good ^ 1);
    let fill = |mut d: MemDisk| {
        let mut mem = Mem::new(64, POOL);
        let mut fs = mount(&mut mem, &mut d).unwrap();
        let f = fs.create(ROOT, b"f").unwrap();
        (0..)
            .take_while(|&p| fs.write(f, p * BLOCK_SIZE as u64, &[1; BLOCK_SIZE]).is_ok())
            .count()
    };
    let mut no_older = disk.clone();
    no_older.durable[1].fill(0);
    assert_eq!(fill(bad), fill(no_older));
}

#[test]
fn small_commits_log_the_bitmap_until_the_log_fills() {
    let mut disk = MemDisk::new(16384);
    let mut mem = Mem::new(16384, POOL);
    let mut fs = format(&mut mem, &mut disk);
    // The newest superblock's log length and first page block.
    let log = |disk: &MemDisk| {
        let sb = (0..2).max_by_key(|&s| le64(&disk.durable[s], 8)).unwrap();
        (
            le64(&disk.durable[sb], 13 * 8),
            le64(&disk.durable[sb], SB_HDR),
        )
    };
    let (_, page) = log(fs.disk());
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[1; 100]).unwrap();
    fs.commit().unwrap();
    let (n, p) = log(fs.disk());
    assert!(n > 0 && p == page, "{n} {p} {page}");
    // Pages in 250 words of the bitmap: more than the log holds, so the pages are written and the log empties.
    for i in 0..250 * 64 {
        fs.write(f, i * BLOCK_SIZE as u64, &[2; 8]).unwrap();
    }
    fs.commit().unwrap();
    let (n, p) = log(fs.disk());
    assert!(n == 0 && p != page, "{n} {p} {page}");
    let tree = snapshot(&mut disk).unwrap();
    assert_eq!(tree.len(), 1);
}

#[test]
fn a_fresh_image_mounts_in_one_request() {
    // The superblocks, the bitmap page and the root leaf are blocks 0 to 3.
    let mut disk = MemDisk::new(1024);
    format(&mut Mem::new(1024, POOL), &mut disk);
    let io = Cell::new([0; 3]);
    let mut mem = Mem::new(1024, POOL);
    let mut fs = mem.fs(Counted(&mut disk, &io));
    fs.mount().unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    assert_eq!(names(&mut fs, ROOT), Vec::<String>::new());
}

#[test]
fn block_io_per_operation() {
    let mut disk = MemDisk::new(1024);
    let mut mem = Mem::new(1024, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    let many = fs.mkdir(ROOT, b"many").unwrap();
    for i in 0..300 {
        fs.create(many, format!("{i:0>100}").as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    let io = Cell::new([0; 3]);
    let mut fs = mem.fs(Counted(&mut disk, &io));
    // [reads, writes, flushes]: blocks 0 to 7 in one request (the superblocks and here the bitmap page, which the
    // older slot shares), then the rightmost path (root and last leaf), past them.
    fs.mount().unwrap();
    assert_eq!(io.take(), [3, 0, 0]);
    assert_eq!(fs.height(), 2);
    // The leaf with the root's entries and the first inodes; the root node is cached.
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    // Every node it changes is cached and stays dirty in memory until the commit.
    let b = fs.create(docs, b"b.txt").unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    // A data page stays in the buffer until the buffer is needed, `map` or a commit: both writes change it there.
    fs.write(b, 0, &[1; 100]).unwrap();
    fs.write(b, 100, &[2; 100]).unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    // The data page and the dirty nodes after it in one request, then the superblock, its log holding the bitmap's
    // changes, between two flushes.
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 2, 2]);
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    let a = fs.lookup(docs, b"a.txt").unwrap();
    fs.read(a, 0, &mut [0; 5]).unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    // Another page needs the buffer: the unwritten one is written first.
    fs.write(b, 0, b"x").unwrap();
    fs.read(a, 0, &mut [0; 5]).unwrap();
    assert_eq!(io.take(), [2, 1, 0]);
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 2, 2]);
    // The entry's leaf and the leaf with its inode.
    fs.lookup(many, format!("{:0>100}", 7).as_bytes()).unwrap();
    assert_eq!(io.take(), [2, 0, 0]);
    // One read per leaf of 300 entries.
    let mut n = 0;
    let end = fs
        .readdir(many, 0, |_, _, _| {
            n += 1;
            false
        })
        .unwrap();
    assert_eq!((n, end, io.take()), (300, u64::MAX, [12, 0, 0]));
    // Renames and unlinks change cached nodes only; the commit after them is one request again.
    fs.rename(docs, b"b.txt", docs, b"c.txt").unwrap();
    fs.rename(docs, b"c.txt", many, b"c.txt").unwrap();
    fs.unlink(many, b"c.txt").unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 2, 2]);
}

#[test]
fn a_one_page_write_and_commit_write_two_blocks() {
    let mut disk = MemDisk::new(1024);
    let mut mem = Mem::new(1024, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[1; 64]).unwrap();
    for name in ["t", "r0"] {
        fs.create(ROOT, name.as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    let io = Cell::new([0; 3]);
    let mut fs = mem.fs(Counted(&mut disk, &io));
    fs.mount().unwrap();
    fs.write(f, 0, &[0; 64]).unwrap();
    fs.commit().unwrap();
    io.take();
    // Each time the page leads the nodes' request, moved to a free run if the blocks after it are taken.
    for i in 1..20 {
        fs.write(f, 0, &[i; 64]).unwrap();
        fs.commit().unwrap();
        assert_eq!(io.take(), [0, 2, 2], "round {i}");
    }
    let mut buf = [0; 64];
    assert_eq!(fs.read(f, 0, &mut buf), Ok(64));
    assert_eq!(buf, [19; 64]);
}

#[test]
fn a_directory_of_100k_entries_lists_in_linear_requests() {
    const N: usize = 100_000;
    let mut disk = MemDisk::new(16384);
    let mut mem = Mem::new(16384, POOL);
    let mut fs = format(&mut mem, &mut disk);
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let name = |i: usize| format!("entry-{i:06}");
    let files: Vec<_> = (0..N)
        .map(|i| fs.create(d, name(i).as_bytes()).unwrap())
        .collect();
    fs.commit().unwrap();
    let io = Cell::new([0; 3]);
    let mut fs = mem.fs(Counted(&mut disk, &io));
    fs.mount().unwrap();
    assert_eq!(fs.height(), 3);
    for (i, f) in files.iter().enumerate() {
        assert_eq!(fs.lookup(d, name(i).as_bytes()), Ok(*f));
    }
    // As the kernel lists: 64 entries a call, resuming from the cursor.
    io.take();
    let (mut cursor, mut listed) = (0, 0);
    while cursor != u64::MAX {
        let mut n = 0;
        cursor = fs
            .readdir(d, cursor, |_, _, _| {
                n += 1;
                n > 64
            })
            .unwrap();
        listed += n.min(64);
    }
    let reads = io.take()[0];
    assert_eq!(listed, N);
    // About one read per leaf of entries (each holds about a hundred), never one per entry.
    assert!(reads < N / 50, "{reads} reads");
    // Unlinking nine in ten merges leaves and internal nodes back together.
    for i in (0..N).filter(|i| i % 10 != 0) {
        fs.unlink(d, name(i).as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    for (i, f) in files.iter().enumerate() {
        let r = fs.lookup(d, name(i).as_bytes());
        assert_eq!(
            r,
            if i % 10 == 0 {
                Ok(*f)
            } else {
                Err(Error::NotFound)
            }
        );
    }
    assert_eq!(names(&mut fs, d).len(), N / 10);
}

/// A sparse host file of `blocks` blocks.
struct FileDisk(std::fs::File, u64);

impl Disk for FileDisk {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        use std::os::unix::fs::FileExt;
        self.0
            .read_exact_at(bufs.as_flattened_mut(), block * BLOCK_SIZE as u64)
            .map_err(|_| Error::Io)
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        use std::os::unix::fs::FileExt;
        self.0
            .write_all_at(bufs.as_flattened(), block * BLOCK_SIZE as u64)
            .map_err(|_| Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.1
    }
}

/// Writes `size` bytes in 1 MiB chunks (each 8-byte word holds its offset), commits every 64 MiB, remounts, and checks
/// every byte, on a sparse file with room for the file and 64 MiB more.
fn big_file(size: u64) {
    const CHUNK: usize = 1 << 20;
    let blocks = size / BLOCK_SIZE as u64 + 16384;
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mogfs-{size}.img"));
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(blocks * BLOCK_SIZE as u64).unwrap();
    let mut mem = Mem::new(blocks as usize, 64);
    let mut fs = mem.fs(FileDisk(file.try_clone().unwrap(), blocks));
    fs.format(SEED).unwrap();
    let f = fs.create(ROOT, b"big").unwrap();
    let mut buf = vec![0u8; CHUNK];
    let fill = |buf: &mut [u8], at: u64| {
        for (i, w) in buf.chunks_mut(8).enumerate() {
            w.copy_from_slice(&(at + 8 * i as u64).to_le_bytes());
        }
    };
    for at in (0..size).step_by(CHUNK) {
        fill(&mut buf, at);
        fs.write(f, at, &buf).unwrap();
        if (at + CHUNK as u64).is_multiple_of(64 << 20) {
            fs.commit().unwrap();
        }
    }
    fs.commit().unwrap();
    let mut fs = mem.fs(FileDisk(file, blocks));
    fs.mount().unwrap();
    let f = fs.lookup(ROOT, b"big").unwrap();
    assert_eq!(fs.stat(f).unwrap().size, size);
    let mut expected = vec![0u8; CHUNK];
    for at in (0..size).step_by(CHUNK) {
        fill(&mut expected, at);
        assert_eq!(fs.read(f, at, &mut buf), Ok(CHUNK));
        assert!(buf == expected, "bytes from {at}");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_1_gib_file_round_trips_on_a_sparse_disk() {
    big_file(1 << 30);
}

#[test]
fn an_older_slot_larger_than_the_memory_is_reserved_without_reading_past_it() {
    let mut disk = MemDisk::new(70000);
    let mut mem = Mem::new(70000, POOL);
    let mut fs = format(&mut mem, &mut disk);
    fs.create(ROOT, b"a").unwrap();
    fs.commit().unwrap();
    // Slot 0 (generation 2) shrinks to 16384 blocks: one bitmap page whose sum covers 256 words, its log folded in.
    let sb = disk.durable[0];
    let page = le64(&sb, SB_HDR) as usize;
    for j in 0..le64(&sb, 13 * 8) as usize {
        let at = SB_HDR + 16 * 3 + 16 * j;
        let w = le64(&sb, at) as usize;
        disk.durable[page][8 * w..8 * w + 8].copy_from_slice(&sb[at + 8..at + 16]);
    }
    disk.durable[0][SB_HDR + 16..BLOCK_SIZE - 8].fill(0);
    let s = sum(page as u64, &disk.durable[page][..2048]);
    let disk = crafted(disk, &[0], 13, 0);
    let disk = crafted(disk, &[0], 16, s);
    let disk = crafted(disk, &[0], 2, 16384);
    // Slot 1 keeps 70000 blocks but puts its root past what memory for 16384 blocks holds.
    let mut disk = crafted(disk, &[1], 9, 69000);
    let mut small = Mem::new(16384, POOL);
    let mut fs = small.fs(&mut disk);
    fs.mount().unwrap();
    assert_eq!(names(&mut fs, ROOT), ["a"]);
}

#[test]
fn a_full_disk_can_still_be_emptied() {
    for blocks in [64, 128, 300] {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks, POOL);
        let mut fs = format(&mut mem, &mut disk);
        let mut n = 0;
        while fs.create(ROOT, format!("{n:0>60}").as_bytes()).is_ok() && fs.commit().is_ok() {
            n += 1;
        }
        assert_eq!(fs.create(ROOT, b"more"), Err(Error::NoSpace), "{blocks}");
        assert!(n > 10, "{blocks}: {n}");
        let mut fs = mount(&mut mem, &mut disk).unwrap();
        for i in 0..n {
            fs.unlink(ROOT, format!("{i:0>60}").as_bytes()).unwrap();
            fs.commit().unwrap();
        }
        fs.create(ROOT, b"room again").unwrap();
        fs.commit().unwrap();
        assert_eq!(snapshot_names(&mut disk), ["room again"], "{blocks}");
    }
}

fn snapshot_names(disk: &mut MemDisk) -> Vec<String> {
    let mut mem = Mem::new(disk.durable.len(), POOL);
    let mut fs = mount(&mut mem, disk).unwrap();
    names(&mut fs, ROOT)
}

#[test]
fn format_can_run_again_on_the_same_fs() {
    let mut disk = hello();
    let mut mem = Mem::new(64, POOL);
    let mut fs = mount(&mut mem, &mut disk).unwrap();
    fs.format(2).unwrap();
    fs.create(ROOT, b"x").unwrap();
    fs.commit().unwrap();
    fs.format(3).unwrap();
    assert_eq!(snapshot(&mut disk), Ok(vec![]));
}

const IMAGE_BLOCKS: usize = 256;

fn image_name(i: usize) -> String {
    format!("{i:0>200}")
}

/// A fixed workload: a height-2 tree with leaf splits, nodes written out early, an extent split, a sum updated in
/// place, a truncate, merges and a rename. `image.bin` is what it wrote with step 39b's format (superblock page list and log).
fn image_workload(disk: &mut MemDisk) {
    let mut mem = Mem::new(IMAGE_BLOCKS, POOL);
    let mut fs = mem.fs(disk);
    fs.set_time(1_000);
    fs.format(SEED).unwrap();
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let big = fs.create(docs, b"big").unwrap();
    fs.write(big, 0, &[7; 9000]).unwrap();
    for i in 0..400 {
        fs.create(ROOT, image_name(i).as_bytes()).unwrap();
    }
    assert_eq!(fs.height(), 2);
    let t = fs.create(docs, b"t").unwrap();
    fs.write(t, 0, &[1; 5000]).unwrap();
    fs.commit().unwrap();
    fs.set_time(2_000);
    fs.write(big, 5000, b"mid").unwrap();
    fs.write(big, 5100, b"again").unwrap();
    fs.truncate(t).unwrap();
    for i in (0..400).filter(|i| i % 4 != 0) {
        fs.unlink(ROOT, image_name(i).as_bytes()).unwrap();
    }
    fs.rename(docs, b"big", ROOT, b"big").unwrap();
    fs.commit().unwrap();
}

#[test]
fn image_mounts_and_is_rewritten_bit_for_bit() {
    let image = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/image.bin")).unwrap();
    let mut disk = MemDisk::new(IMAGE_BLOCKS);
    image_workload(&mut disk);
    let differs =
        (0..IMAGE_BLOCKS).find(|&b| disk.durable[b][..] != image[b * BLOCK_SIZE..][..BLOCK_SIZE]);
    assert_eq!(differs, None, "first block that differs");
    let mut disk = MemDisk::new(IMAGE_BLOCKS);
    for (b, block) in disk.durable.iter_mut().zip(image.chunks(BLOCK_SIZE)) {
        b.copy_from_slice(block);
    }
    let mut big = vec![7; 9000];
    big[5000..5003].copy_from_slice(b"mid");
    big[5100..5105].copy_from_slice(b"again");
    let mut want: Tree = (0..400)
        .step_by(4)
        .map(|i| entry(&format!("/{}", image_name(i)), b""))
        .collect();
    want.extend([
        entry("/big", &big),
        entry("/docs/", b""),
        entry("/docs/t", b""),
    ]);
    want.sort();
    assert_eq!(snapshot(&mut disk), Ok(want));
}

/// A sparse in-memory disk of `.1` blocks: unwritten blocks read as zeros. Counts [reads, writes, flushes].
struct Sparse(std::collections::HashMap<u64, Buf>, u64, [usize; 3]);

impl Disk for &mut Sparse {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        self.2[0] += 1;
        for (i, buf) in bufs.iter_mut().enumerate() {
            *buf = self
                .0
                .get(&(block + i as u64))
                .copied()
                .unwrap_or([0; BLOCK_SIZE]);
        }
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        self.2[1] += 1;
        for (i, buf) in bufs.iter().enumerate() {
            self.0.insert(block + i as u64, *buf);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.2[2] += 1;
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.1
    }
}

#[test]
fn a_300_gib_file_system_lists_its_bitmap_through_an_index() {
    // 2400 bitmap pages: two index levels, 10 blocks under the root.
    let blocks = 300 << 18;
    let mut disk = Sparse(Default::default(), blocks, [0; 3]);
    let mut mem = Mem::new(blocks as usize, POOL);
    let mut fs = mem.fs(&mut disk);
    fs.format(SEED).unwrap();
    // Enough commits to fill the log more than once, so pages and index blocks are rewritten.
    let mut files = vec![];
    for i in 0..400 {
        let f = fs.create(ROOT, format!("f{i}").as_bytes()).unwrap();
        fs.write(f, 0, &[i as u8; 5000]).unwrap();
        fs.commit().unwrap();
        files.push(f);
    }
    // 70 MiB touch more bitmap words than the log holds: the commit rewrites the page and the index above it.
    let root = |d: &Sparse| {
        let sb = (0..2).max_by_key(|&s| le64(&d.0[&s], 8)).unwrap();
        (le64(&d.0[&sb], 14 * 8), le64(&d.0[&sb], SB_HDR))
    };
    let before = root(fs.disk());
    let big = fs.create(ROOT, b"big").unwrap();
    for i in 0..70u64 {
        fs.write(big, i << 20, &[i as u8 + 1; 1 << 20]).unwrap();
    }
    fs.commit().unwrap();
    let after = root(fs.disk());
    assert!(
        before.0 == 2 && after.0 == 2 && after.1 != before.1,
        "{before:?} {after:?}"
    );
    fs.disk().2 = [0; 3];
    fs.write(files[7], 0, b"small").unwrap();
    fs.commit().unwrap();
    // A small commit: the data page and the nodes in one request, the superblock between two flushes.
    assert_eq!(fs.disk().2[1..], [2, 2]);
    let mut mem = Mem::new(blocks as usize, POOL);
    let mut fs = mem.fs(&mut disk);
    fs.mount().unwrap();
    let mut buf = [0; 5000];
    for i in 0..400 {
        let name = format!("f{i}");
        let f = fs.lookup(ROOT, name.as_bytes()).unwrap();
        assert_eq!(fs.read(f, 0, &mut buf), Ok(5000));
        assert_eq!(buf[4], if i == 7 { b'l' } else { i as u8 }, "{name}");
    }
    let big = fs.lookup(ROOT, b"big").unwrap();
    for i in [0u64, 33, 69] {
        assert_eq!(fs.read(big, (i << 20) + 7, &mut buf[..1]), Ok(1));
        assert_eq!(buf[0], i as u8 + 1);
    }
}
