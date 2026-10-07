use std::cell::Cell;

use mogfs::{BLOCK_SIZE, Disk, Error, Fs, Inode, Kind, MAX_FILE_SIZE, MAX_INODES, NAME_MAX, ROOT};

type Block = [u8; BLOCK_SIZE];

/// In-memory disk with a write-back cache: writes stay pending until a flush makes them durable. After `cut` events
/// (block writes and flushes) every write and flush fails, as if power were lost there.
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

type Tree = Vec<(String, Vec<u8>)>;

/// Every path under `dir` with its contents; directories end in `/`.
fn walk<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &str, out: &mut Tree) -> Result<(), Error> {
    let mut entries = Vec::new();
    fs.readdir(dir, |name, inode| {
        entries.push((String::from_utf8(name.to_vec()).unwrap(), inode))
    })?;
    for (name, inode) in entries {
        let path = format!("{path}/{name}");
        if fs.kind(inode)? == Kind::Dir {
            out.push((format!("{path}/"), Vec::new()));
            walk(fs, inode, &path, out)?;
        } else {
            let mut buf = vec![0; MAX_FILE_SIZE as usize];
            let n = fs.read(inode, 0, &mut buf)?;
            out.push((path, buf[..n].to_vec()));
        }
    }
    Ok(())
}

fn mount(disk: &mut MemDisk) -> Result<Fs<&mut MemDisk>, Error> {
    let mut fs = Fs::new(disk);
    fs.mount()?;
    Ok(fs)
}

fn format(disk: &mut MemDisk) -> Fs<&mut MemDisk> {
    let mut fs = Fs::new(disk);
    fs.format().unwrap();
    fs
}

fn snapshot(disk: &mut MemDisk) -> Result<Tree, Error> {
    let mut fs = mount(disk)?;
    let mut out = Vec::new();
    walk(&mut fs, ROOT, "", &mut out)?;
    Ok(out)
}

fn entry(path: &str, data: &[u8]) -> (String, Vec<u8>) {
    (path.to_string(), data.to_vec())
}

/// Formats and commits `/docs/a.txt` = "hello": generation 1 (empty) in slot 1, generation 2 in slot 0.
fn hello() -> MemDisk {
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
    let docs = fs.mkdir(ROOT, b"docs").unwrap();
    let a = fs.create(docs, b"a.txt").unwrap();
    fs.write(a, 0, b"hello").unwrap();
    fs.commit().unwrap();
    disk
}

fn hello_tree() -> Tree {
    vec![entry("/docs/", b""), entry("/docs/a.txt", b"hello")]
}

#[test]
fn round_trip_survives_remount_and_drops_uncommitted_changes() {
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
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

    let mut fs = mount(&mut disk).unwrap();
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

#[test]
fn truncate_empties_a_file_and_frees_its_blocks() {
    let mut disk = MemDisk::new(16);
    let mut fs = format(&mut disk);
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
fn flipped_byte_in_any_reachable_block_reads_as_corrupt_or_falls_back() {
    let disk = hello();
    let mut corrupt = 0;
    let mut fallback = 0;
    for block in 2..disk.durable.len() {
        let mut bad = disk.clone();
        bad.durable[block][100] ^= 1;
        match snapshot(&mut bad) {
            Err(Error::Corrupt) => corrupt += 1,
            Ok(t) if t.is_empty() => fallback += 1,
            other => assert_eq!(other, Ok(hello_tree()), "block {block}"),
        }
    }
    // Both directories and the file read as corrupt; a corrupt inode table falls back to the older slot.
    assert_eq!((corrupt, fallback), (3, 1));
}

#[test]
fn mount_falls_back_to_the_older_superblock() {
    let mut disk = hello();
    let mut fs = mount(&mut disk).unwrap();
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
    let mut fs = mount(&mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    fs.create(docs, b"b").unwrap();
    fs.mkdir(ROOT, b"c").unwrap();
    // Slot 0 holds hello; slot 1 the empty file system from `format`.
    let mut bad = disk.clone();
    bad.durable[0][0] ^= 1;
    assert_eq!(snapshot(&mut bad), Ok(vec![]));

    let mut fs = mount(&mut disk).unwrap();
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    fs.create(docs, b"b").unwrap();
    fs.mkdir(ROOT, b"c").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"d").unwrap();
    fs.commit().unwrap();
    // Generation 4 is in slot 0; generation 3, whose blocks stayed reserved through 4's changes, in slot 1.
    disk.durable[0][0] ^= 1;
    let mut tree = hello_tree();
    tree.insert(2, entry("/docs/b", b""));
    tree.push(entry("/c/", b""));
    assert_eq!(snapshot(&mut disk), Ok(tree));
}

/// Rewrites `block`'s checksum (the format's hash, restated) so a crafted change passes the check.
fn reseal(disk: &mut MemDisk, block: usize) {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    let buf = &mut disk.durable[block];
    let mut lanes: [u64; 16] = std::array::from_fn(|i| i as u64);
    lanes[0] ^= block as u64 | 1 << 40;
    for (i, w) in buf[..BLOCK_SIZE - 8].chunks(8).enumerate() {
        lanes[i % 16] = mix(lanes[i % 16], u64::from_le_bytes(w.try_into().unwrap()));
    }
    let sum = lanes.into_iter().fold(0, mix);
    buf[BLOCK_SIZE - 8..].copy_from_slice(&sum.to_le_bytes());
}

/// `disk` with `value` written at `at` in each of `blocks`, resealed.
fn crafted(mut disk: MemDisk, blocks: &[usize], at: usize, value: &[u8]) -> MemDisk {
    for &b in blocks {
        disk.durable[b][at..at + value.len()].copy_from_slice(value);
        reseal(&mut disk, b);
    }
    disk
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

#[test]
fn crafted_superblocks_are_rejected() {
    // The next commit's generation would overflow.
    let mut disk = crafted(hello(), &[0, 1], 8, &u64::MAX.to_le_bytes());
    assert_eq!(snapshot(&mut disk), Err(Error::Corrupt));
    // Block counts too small to hold the slots and a table, or past the disk.
    for blocks in [0u32, 1, 2, 65] {
        let mut disk = crafted(hello(), &[0, 1], 16, &blocks.to_le_bytes());
        assert_eq!(snapshot(&mut disk), Err(Error::Corrupt), "{blocks}");
    }
    // A table block at a superblock slot or past the end.
    for ptr in [1u32, 64] {
        let mut disk = crafted(hello(), &[0, 1], 20, &ptr.to_le_bytes());
        assert_eq!(snapshot(&mut disk), Err(Error::Corrupt), "{ptr}");
    }
    // A generation in the wrong slot: slot 0 is skipped, slot 1 (generation 1, empty) is used.
    let mut disk = crafted(hello(), &[0], 8, &3u64.to_le_bytes());
    assert_eq!(snapshot(&mut disk), Ok(vec![]));
    assert_eq!(Fs::new(&mut MemDisk::new(2)).format(), Err(Error::NoSpace));
}

#[test]
fn crafted_records_fall_back_and_crafted_entries_are_corrupt() {
    let disk = hello();
    // Slot 0 holds hello; inode 1 is `docs`, inode 2 `a.txt`, 64-byte records.
    let table = le32(&disk.durable[0], 20);
    let docs = le32(&disk.durable[table], 64 + 8);
    // Control: a legal crafted size passes the checksum.
    let mut ok = crafted(disk.clone(), &[table], 128 + 4, &3u32.to_le_bytes());
    assert_eq!(
        snapshot(&mut ok),
        Ok(vec![entry("/docs/", b""), entry("/docs/a.txt", b"hel")])
    );
    // Invalid inode records make the newest slot unusable, so mount falls back to the empty older one.
    let records = [
        // A size past the file limit, or a directory size that is not whole entries.
        (128 + 4, MAX_FILE_SIZE as u32 + 1),
        (64 + 4, 57),
        // A data block at a superblock slot or past the end, or one already used by `docs`.
        (128 + 8, 1),
        (128 + 8, 64),
        (128 + 8, docs as u32),
    ];
    for (at, value) in records {
        let mut bad = crafted(disk.clone(), &[table], at, &value.to_le_bytes());
        assert_eq!(snapshot(&mut bad), Ok(vec![]), "{at} {value}");
    }
    // A directory with no block for its entries, an entry naming an inode past the table, too long a name, a bad name.
    let mut bad = crafted(disk.clone(), &[table], 64 + 8, &0u32.to_le_bytes());
    assert_eq!(snapshot(&mut bad), Err(Error::Corrupt));
    let entries: [(usize, &[u8]); 4] = [
        (0, &MAX_INODES.to_le_bytes()),
        (4, &[NAME_MAX as u8 + 1]),
        (4, &[0]),
        (5, b"/"),
    ];
    for (at, value) in entries {
        let mut bad = crafted(disk.clone(), &[docs], at, value);
        assert_eq!(snapshot(&mut bad), Err(Error::Corrupt), "{at}");
    }
}

#[test]
fn older_slot_past_the_newest_block_count_is_ignored() {
    let mut disk = hello();
    // The older slot (1, generation 1) has its table at block 2; point the root's blocks past 20 and shrink slot 0
    // to 20 blocks.
    assert_eq!(le32(&disk.durable[1], 20), 2);
    for p in 0..14 {
        disk.durable[2][8 + 4 * p..][..4].copy_from_slice(&(30 + p as u32).to_le_bytes());
    }
    reseal(&mut disk, 2);
    let mut disk = crafted(disk, &[0], 16, &20u32.to_le_bytes());
    let mut fs = mount(&mut disk).unwrap();
    fs.create(ROOT, b"x").unwrap();
    fs.commit().unwrap();
    let mut tree = hello_tree();
    tree.push(entry("/x", b""));
    assert_eq!(snapshot(&mut disk), Ok(tree));
}

/// Fails the first read of block 0.
struct FailOnce<'a>(&'a mut MemDisk, bool);

impl Disk for FailOnce<'_> {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        if block == 0 && !self.1 {
            self.1 = true;
            return Err(Error::Io);
        }
        (&mut *self.0).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
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
    let mut fs = Fs::new(FailOnce(&mut disk, false));
    assert_eq!(fs.mount(), Err(Error::Io));
    fs.mount().unwrap();
    fs.create(ROOT, b"new").unwrap();
    fs.commit().unwrap();
    let mut tree = hello_tree();
    tree.push(entry("/new", b""));
    assert_eq!(snapshot(&mut disk), Ok(tree));
}

/// Mounts `disk`, overwrites `/docs/a.txt`, adds `/src` and `/docs/b.txt`, and commits.
fn change(disk: &mut MemDisk) -> Result<(), Error> {
    let mut fs = mount(disk)?;
    let docs = fs.lookup(ROOT, b"docs")?;
    let a = fs.lookup(docs, b"a.txt")?;
    fs.write(a, 0, b"HE")?;
    fs.mkdir(ROOT, b"src")?;
    let b = fs.create(docs, b"b.txt")?;
    fs.write(b, 0, &[7; 5000])?;
    fs.commit()
}

#[test]
fn power_cut_anywhere_leaves_the_old_or_the_new_state() {
    let base = hello();
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
    let mut disk = MemDisk::new(16);
    let mut fs = format(&mut disk);
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
    let mut fs = format(&mut disk);
    let long = [b'x'; NAME_MAX + 1];
    for bad in [&long[..], b"", b".", b"..", b"a/b", b"a\0"] {
        assert_eq!(fs.create(ROOT, bad), Err(Error::InvalidName));
    }
    let f = fs.create(ROOT, &long[..NAME_MAX]).unwrap();
    assert_eq!(fs.mkdir(ROOT, &long[..NAME_MAX]), Err(Error::Exists));
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
}

#[test]
fn inodes_run_out() {
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
    for i in 1..MAX_INODES {
        fs.create(ROOT, format!("{i}").as_bytes()).unwrap();
    }
    assert_eq!(fs.create(ROOT, b"one-more"), Err(Error::NoSpace));
    fs.commit().unwrap();
}

#[test]
fn no_space_changes_nothing() {
    let mut disk = MemDisk::new(8);
    let mut fs = format(&mut disk);
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, &[7; 4088]).unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.write(f, 0, &[1; 4 * 4088]), Err(Error::NoSpace));
    let mut b = [0; 1];
    fs.read(f, 0, &mut b).unwrap();
    assert_eq!(b[0], 7);
    assert_eq!(
        fs.write(f, 0, &[1; MAX_FILE_SIZE as usize]),
        Err(Error::NoSpace)
    );
    fs.commit().unwrap();
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/f", &[7; 4088])]));
}

/// Counts the reads, writes and flushes that reach `disk`.
struct Counted<'a>(&'a mut MemDisk, &'a Cell<[usize; 3]>);

impl Counted<'_> {
    fn bump(&self, i: usize, n: usize) {
        let mut c = self.1.get();
        c[i] += n;
        self.1.set(c);
    }
}

impl Disk for Counted<'_> {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        self.bump(0, bufs.len());
        (&mut *self.0).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
        self.bump(1, bufs.len());
        (&mut *self.0).write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.bump(2, 1);
        (&mut *self.0).flush()
    }

    fn blocks(&self) -> u64 {
        self.0.durable.len() as u64
    }
}

#[test]
fn block_io_per_operation() {
    let mut disk = hello();
    let io = Cell::new([0; 3]);
    let mut fs = Fs::new(Counted(&mut disk, &io));
    fs.mount().unwrap();
    // [reads, writes, flushes]: both superblocks, then each slot's inode table.
    assert_eq!(io.take(), [4, 0, 0]);
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    // Reads the directory block it scans and appends to, then writes it; the inode table waits for the commit.
    let b = fs.create(docs, b"b.txt").unwrap();
    assert_eq!(io.take(), [1, 1, 0]);
    fs.write(b, 0, &[1; 100]).unwrap();
    assert_eq!(io.take(), [0, 1, 0]);
    // The block it appends to is still buffered.
    fs.write(b, 100, &[2; 100]).unwrap();
    assert_eq!(io.take(), [0, 1, 0]);
    // The inode table block, then the superblock between two flushes.
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 2, 2]);
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    let a = fs.lookup(docs, b"a.txt").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    fs.read(a, 0, &mut [0; 5]).unwrap();
    assert_eq!(io.take(), [1, 0, 0]);

    // A two-block directory: 73 entries fill the first block.
    let dir = fs.mkdir(ROOT, b"many").unwrap();
    for i in 0..74 {
        fs.create(dir, format!("{i}").as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    io.take();
    fs.lookup(dir, b"73").unwrap();
    assert_eq!(io.take(), [2, 0, 0]);
    fs.lookup(dir, b"0").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    // Scans both blocks (the first still buffered) and appends to the second, now buffered.
    fs.create(dir, b"74").unwrap();
    assert_eq!(io.take(), [1, 1, 0]);
}
