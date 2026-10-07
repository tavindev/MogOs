use std::cell::Cell;

use mogfs::{BLOCK_SIZE, Disk, Error, Fs, Inode, MAX_FILE_SIZE, MAX_INODES, NAME_MAX, ROOT};

/// In-memory disk; the write after `cut` writes is torn (first half only) and every later write or flush fails.
#[derive(Clone)]
struct MemDisk {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    writes: usize,
    cut: usize,
}

impl MemDisk {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; BLOCK_SIZE]; blocks],
            writes: 0,
            cut: usize::MAX,
        }
    }

    fn with_cut(&self, cut: usize) -> Self {
        Self {
            writes: 0,
            cut,
            ..self.clone()
        }
    }
}

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
        *buf = *self.blocks.get(block as usize).ok_or(Error::Io)?;
        Ok(())
    }

    fn write(&mut self, block: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), Error> {
        let b = self.blocks.get_mut(block as usize).ok_or(Error::Io)?;
        self.writes += 1;
        if self.writes > self.cut {
            if self.writes == self.cut + 1 {
                b[..BLOCK_SIZE / 2].copy_from_slice(&buf[..BLOCK_SIZE / 2]);
            }
            return Err(Error::Io);
        }
        *b = *buf;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.writes > self.cut {
            Err(Error::Io)
        } else {
            Ok(())
        }
    }

    fn blocks(&self) -> u64 {
        self.blocks.len() as u64
    }
}

type Tree = Vec<(String, Vec<u8>)>;

/// Every path under `dir` with its contents; directories end in `/`.
fn walk(fs: &mut Fs<&mut MemDisk>, dir: Inode, path: &str, out: &mut Tree) -> Result<(), Error> {
    let mut entries = Vec::new();
    fs.readdir(dir, |name, inode| {
        entries.push((String::from_utf8(name.to_vec()).unwrap(), inode))
    })?;
    for (name, inode) in entries {
        let path = format!("{path}/{name}");
        let mut buf = vec![0; MAX_FILE_SIZE as usize];
        match fs.read(inode, 0, &mut buf) {
            Ok(n) => out.push((path, buf[..n].to_vec())),
            Err(Error::IsDir) => {
                out.push((format!("{path}/"), Vec::new()));
                walk(fs, inode, &path, out)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn snapshot(disk: &mut MemDisk) -> Result<Tree, Error> {
    let mut fs = Fs::mount(disk)?;
    let mut out = Vec::new();
    walk(&mut fs, ROOT, "", &mut out)?;
    Ok(out)
}

fn entry(path: &str, data: &[u8]) -> (String, Vec<u8>) {
    (path.to_string(), data.to_vec())
}

/// Formats and commits `/docs/a.txt` = "hello".
fn hello() -> MemDisk {
    let mut disk = MemDisk::new(64);
    let mut fs = Fs::format(&mut disk).unwrap();
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    fs.commit().unwrap();
    disk
}

#[test]
fn round_trip_survives_remount_and_drops_uncommitted_changes() {
    let mut disk = MemDisk::new(64);
    let mut fs = Fs::format(&mut disk).unwrap();
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    let big = fs.create(docs, b"big").unwrap();
    fs.write(big, 0, &[1; 5000]).unwrap();
    fs.write(big, 4050, &[2; 100]).unwrap();
    fs.mkdir(ROOT, b"empty").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"lost").unwrap();

    let mut expected = vec![1; 5000];
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

    let mut fs = Fs::mount(&mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    let a = fs.lookup(docs, b"a.txt").unwrap();
    let mut buf = [0; 8];
    assert_eq!(fs.read(a, 1, &mut buf), Ok(4));
    assert_eq!(&buf[..4], b"ello");
    assert_eq!(fs.read(a, 9, &mut buf), Ok(0));
}

#[test]
fn flipped_byte_in_any_reachable_block_reads_as_corrupt() {
    let disk = hello();
    let good = snapshot(&mut disk.clone()).unwrap();
    let mut corrupt = 0;
    for block in 2..disk.blocks.len() {
        let mut bad = disk.clone();
        bad.blocks[block][100] ^= 1;
        match snapshot(&mut bad) {
            Err(Error::Corrupt) => corrupt += 1,
            other => assert_eq!(other, Ok(good.clone()), "block {block}"),
        }
    }
    // The inode table, both directories and the file.
    assert_eq!(corrupt, 4);
}

#[test]
fn mount_falls_back_to_the_older_superblock() {
    let mut disk = hello();
    let mut fs = Fs::mount(&mut disk).unwrap();
    fs.create(ROOT, b"b").unwrap();
    fs.commit().unwrap();
    // Generation 3 is in slot 1; generation 2 (hello) in slot 0.
    let mut bad = disk.clone();
    bad.blocks[1][BLOCK_SIZE - 1] ^= 1;
    assert_eq!(snapshot(&mut bad), snapshot(&mut hello()));
    bad.blocks[0][0] ^= 1;
    assert_eq!(snapshot(&mut bad).err(), Some(Error::Corrupt));
}

/// Rewrites `block`'s checksum (the format's hash, restated) so a crafted change passes the check.
fn reseal(disk: &mut MemDisk, block: usize) {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    let buf = &mut disk.blocks[block];
    let mut lanes = [block as u64, 1, 2, 3];
    for (i, w) in buf[..BLOCK_SIZE - 8].chunks(8).enumerate() {
        lanes[i % 4] = mix(lanes[i % 4], u64::from_le_bytes(w.try_into().unwrap()));
    }
    let sum = lanes.into_iter().fold(0, mix);
    buf[BLOCK_SIZE - 8..].copy_from_slice(&sum.to_le_bytes());
}

/// `hello` with `value` written at `at` in `block` (or both superblock slots for `None`), resealed.
fn crafted(block: Option<usize>, at: usize, value: &[u8]) -> MemDisk {
    let mut disk = hello();
    for b in block.map_or(vec![0, 1], |b| vec![b]) {
        disk.blocks[b][at..at + value.len()].copy_from_slice(value);
        reseal(&mut disk, b);
    }
    disk
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

#[test]
fn crafted_superblocks_are_corrupt() {
    // The next commit's generation would overflow.
    let mut disk = crafted(None, 8, &u64::MAX.to_le_bytes());
    assert_eq!(snapshot(&mut disk).err(), Some(Error::Corrupt));
    // Block counts too small to hold the slots and a table (free space would underflow), or past the disk.
    for blocks in [0u32, 1, 2, 65] {
        let mut disk = crafted(None, 16, &blocks.to_le_bytes());
        assert_eq!(snapshot(&mut disk).err(), Some(Error::Corrupt), "{blocks}");
    }
    // A table block at a superblock slot or past the end.
    for ptr in [1u32, 64] {
        let mut disk = crafted(None, 20, &ptr.to_le_bytes());
        assert_eq!(snapshot(&mut disk).err(), Some(Error::Corrupt), "{ptr}");
    }
    assert_eq!(Fs::format(&mut MemDisk::new(2)).err(), Some(Error::NoSpace));
}

#[test]
fn crafted_records_and_entries_are_corrupt() {
    let disk = hello();
    // Slot 0 holds hello; inode 1 is `docs`, inode 2 `a.txt`, 64-byte records.
    let table = le32(&disk.blocks[0], 20);
    let docs = le32(&disk.blocks[table], 64 + 8);
    let bad = [
        // A size past the file limit.
        (table, 128 + 4, MAX_FILE_SIZE as u32 + 1),
        // A data block at a superblock slot or past the end.
        (table, 128 + 8, 1),
        (table, 128 + 8, 64),
        // A directory entry naming an inode past the table, or a name longer than NAME_MAX.
        (docs, 0, MAX_INODES),
        (docs, 4, NAME_MAX as u32 + 1),
    ];
    // Control: a legal crafted size passes the checksum.
    let mut disk = crafted(Some(table), 128 + 4, &3u32.to_le_bytes());
    assert_eq!(
        snapshot(&mut disk),
        Ok(vec![entry("/docs/", b""), entry("/docs/a.txt", b"hel")])
    );
    for (block, at, value) in bad {
        let mut disk = crafted(Some(block), at, &value.to_le_bytes());
        assert_eq!(
            snapshot(&mut disk).err(),
            Some(Error::Corrupt),
            "{block} {at}"
        );
    }
}

#[test]
fn uncommitted_changes_leave_the_older_slot_intact() {
    let mut disk = hello();
    let mut fs = Fs::mount(&mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    fs.create(docs, b"b").unwrap();
    fs.mkdir(ROOT, b"c").unwrap();
    // Slot 0 holds hello; slot 1 the empty file system from `format`.
    disk.blocks[0][0] ^= 1;
    assert_eq!(snapshot(&mut disk), Ok(vec![]));
}

#[test]
fn power_cut_at_any_write_leaves_the_old_or_the_new_state() {
    let base = hello();
    let change = |disk: &mut MemDisk| -> Result<(), Error> {
        let mut fs = Fs::mount(disk)?;
        let docs = fs.lookup(ROOT, b"docs")?;
        let a = fs.lookup(docs, b"a.txt")?;
        fs.write(a, 0, b"HE")?;
        fs.mkdir(ROOT, b"src")?;
        let b = fs.create(docs, b"b.txt")?;
        fs.write(b, 0, &[7; 5000])?;
        fs.commit()
    };
    let old = snapshot(&mut base.clone()).unwrap();
    let mut full = base.with_cut(usize::MAX);
    change(&mut full).unwrap();
    let new = snapshot(&mut full).unwrap();
    assert_ne!(old, new);

    for cut in 0..full.writes {
        let mut disk = base.with_cut(cut);
        assert!(change(&mut disk).is_err(), "cut {cut}");
        disk.cut = usize::MAX;
        assert_eq!(snapshot(&mut disk), Ok(old.clone()), "cut {cut}");
    }
    let mut disk = base.with_cut(full.writes);
    assert_eq!(change(&mut disk), Ok(()));
    assert_eq!(snapshot(&mut disk), Ok(new));
}

#[test]
fn commits_reclaim_replaced_blocks() {
    let mut disk = MemDisk::new(16);
    let mut fs = Fs::format(&mut disk).unwrap();
    let f = fs.create(ROOT, b"f").unwrap();
    for i in 0..100 {
        fs.write(f, 0, &[i; 9000]).unwrap();
        fs.commit().unwrap();
    }
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/f", &[99; 9000])]));
}

#[test]
fn limits_and_misuse_return_errors() {
    let mut disk = MemDisk::new(64);
    let mut fs = Fs::format(&mut disk).unwrap();
    let long = [b'x'; NAME_MAX + 1];
    for bad in [&long[..], b"", b".", b"..", b"a/b", b"a\0"] {
        assert_eq!(fs.create(ROOT, bad), Err(Error::InvalidName));
    }
    let f = fs.create(ROOT, &long[..NAME_MAX]).unwrap();
    assert_eq!(fs.mkdir(ROOT, &long[..NAME_MAX]), Err(Error::Exists));
    assert_eq!(fs.lookup(ROOT, b"missing"), Err(Error::NotFound));
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
}

#[test]
fn inodes_run_out() {
    let mut disk = MemDisk::new(64);
    let mut fs = Fs::format(&mut disk).unwrap();
    for i in 1..MAX_INODES {
        fs.create(ROOT, format!("{i}").as_bytes()).unwrap();
    }
    assert_eq!(fs.create(ROOT, b"one-more"), Err(Error::NoSpace));
    fs.commit().unwrap();
}

#[test]
fn full_disk_returns_no_space_and_keeps_the_committed_state() {
    let mut disk = MemDisk::new(8);
    let mut fs = Fs::format(&mut disk).unwrap();
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, b"kept").unwrap();
    fs.commit().unwrap();
    assert_eq!(
        fs.write(f, 0, &[1; MAX_FILE_SIZE as usize]),
        Err(Error::NoSpace)
    );
    assert_eq!(fs.create(ROOT, b"g"), Err(Error::NoSpace));
    fs.commit().unwrap();
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/f", b"kept")]));
}

/// Counts the reads, writes and flushes that reach `disk`.
struct Counted<'a>(&'a mut MemDisk, &'a Cell<[usize; 3]>);

impl Counted<'_> {
    fn bump(&self, i: usize) {
        let mut c = self.1.get();
        c[i] += 1;
        self.1.set(c);
    }
}

impl Disk for Counted<'_> {
    fn read(&mut self, block: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
        self.bump(0);
        (&mut *self.0).read(block, buf)
    }

    fn write(&mut self, block: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), Error> {
        self.bump(1);
        (&mut *self.0).write(block, buf)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.bump(2);
        (&mut *self.0).flush()
    }

    fn blocks(&self) -> u64 {
        self.0.blocks.len() as u64
    }
}

#[test]
fn block_io_per_operation() {
    let mut disk = hello();
    let io = Cell::new([0; 3]);
    let mut fs = Fs::mount(Counted(&mut disk, &io)).unwrap();
    // [reads, writes, flushes]: both superblocks, then each slot's inode table.
    assert_eq!(io.take(), [4, 0, 0]);
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    // Reads the directory block it scans and appends to; writes it and the inode table block.
    let b = fs.create(docs, b"b.txt").unwrap();
    assert_eq!(io.take(), [1, 2, 0]);
    fs.write(b, 0, &[1; 100]).unwrap();
    assert_eq!(io.take(), [0, 2, 0]);
    // The block it appends to is still buffered.
    fs.write(b, 100, &[2; 100]).unwrap();
    assert_eq!(io.take(), [0, 2, 0]);
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 1, 2]);
    let a = fs.lookup(docs, b"a.txt").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    fs.read(a, 0, &mut [0; 5]).unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
}
