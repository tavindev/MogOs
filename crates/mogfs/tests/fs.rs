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
fn the_disk_can_be_replaced_before_mount() {
    let (mut a, mut b) = (hello(), MemDisk::new(64));
    let mut fs = Fs::new(&mut b);
    *fs.disk() = &mut a;
    fs.mount().unwrap();
    assert!(fs.lookup(ROOT, b"docs").is_ok());
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

fn names<D: Disk>(fs: &mut Fs<D>, dir: Inode) -> Vec<String> {
    let mut out = Vec::new();
    fs.readdir(dir, 0, |n, _, _| {
        out.push(String::from_utf8(n.to_vec()).unwrap());
        false
    })
    .unwrap();
    out
}

#[test]
fn unlink_removes_files_and_empty_directories_and_frees_their_space() {
    let mut disk = MemDisk::new(20);
    let mut fs = format(&mut disk);
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
    // The last entry moves into the freed slot.
    fs.unlink(ROOT, b"a").unwrap();
    assert_eq!(names(&mut fs, ROOT), ["d", "b"]);
    assert_eq!(fs.kind(a), Err(Error::NotFound));
    fs.unlink(d, b"x").unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    // Freed inodes are reused; blocks of committed and uncommitted files are freed.
    for _ in 0..100 {
        let f = fs.create(ROOT, b"f").unwrap();
        assert_eq!(f, a);
        fs.write(f, 0, &[2; 9000]).unwrap();
        fs.commit().unwrap();
        fs.unlink(ROOT, b"f").unwrap();
    }
    fs.commit().unwrap();
    assert_eq!(snapshot(&mut disk), Ok(vec![entry("/b", b"")]));
}

#[test]
fn unlink_in_a_two_block_directory_frees_the_emptied_block() {
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
    let dir = fs.mkdir(ROOT, b"many").unwrap();
    for i in 0..74 {
        fs.create(dir, format!("{i}").as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    fs.unlink(dir, b"0").unwrap();
    fs.unlink(dir, b"72").unwrap();
    fs.commit().unwrap();
    let mut fs = mount(&mut disk).unwrap();
    let dir = fs.lookup(ROOT, b"many").unwrap();
    let mut expected: Vec<String> = (0..73).map(|i| i.to_string()).collect();
    expected[0] = "73".to_string();
    expected.retain(|n| n != "72");
    assert_eq!(names(&mut fs, dir), expected);
}

#[test]
fn rename_moves_entries_within_and_across_directories() {
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
    let a = fs.mkdir(ROOT, b"a").unwrap();
    let b = fs.mkdir(a, b"b").unwrap();
    let f = fs.create(ROOT, b"f").unwrap();
    fs.write(f, 0, b"data").unwrap();
    fs.create(ROOT, b"g").unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.rename(ROOT, b"f", ROOT, b"g"), Err(Error::Exists));
    assert_eq!(fs.rename(ROOT, b"no", ROOT, b"h"), Err(Error::NotFound));
    assert_eq!(fs.rename(ROOT, b"f", f, b"h"), Err(Error::NotDir));
    assert_eq!(fs.rename(ROOT, b"f", ROOT, b"/"), Err(Error::InvalidName));
    // A directory cannot move into itself or below itself.
    assert_eq!(fs.rename(ROOT, b"a", a, b"x"), Err(Error::InvalidName));
    assert_eq!(fs.rename(ROOT, b"a", b, b"x"), Err(Error::InvalidName));
    fs.rename(ROOT, b"f", ROOT, b"h").unwrap();
    fs.rename(ROOT, b"h", b, b"f").unwrap();
    fs.rename(a, b"b", ROOT, b"b").unwrap();
    assert_eq!(fs.lookup(ROOT, b"b"), Ok(b));
    fs.commit().unwrap();
    assert_eq!(
        snapshot(&mut disk),
        Ok(vec![
            entry("/a/", b""),
            entry("/g", b""),
            entry("/b/", b""),
            entry("/b/f", b"data"),
        ])
    );
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
fn the_last_generation_commits_and_then_commits_fail() {
    let mut disk = crafted(hello(), &[0], 8, &(u64::MAX - 1).to_le_bytes());
    let mut fs = mount(&mut disk).unwrap();
    fs.create(ROOT, b"new").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"lost").unwrap();
    assert_eq!(fs.commit(), Err(Error::Corrupt));
    let mut tree = hello_tree();
    tree.push(entry("/new", b""));
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(tree));
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
        // A data block past the file's size, or a free inode with a size.
        (128 + 12, 40),
        (192 + 4, 5),
    ];
    for (at, value) in records {
        let mut bad = crafted(disk.clone(), &[table], at, &value.to_le_bytes());
        assert_eq!(snapshot(&mut bad), Ok(vec![]), "{at} {value}");
    }
    // A directory with no block for its entries, an entry naming an inode past the table, too long a name, a bad name.
    let mut bad = crafted(disk.clone(), &[table], 64 + 8, &0u32.to_le_bytes());
    assert_eq!(snapshot(&mut bad), Err(Error::Corrupt));
    // An entry naming a free inode.
    let entries: [(usize, &[u8]); 5] = [
        (0, &3u32.to_le_bytes()),
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

/// Entries naming the root, the directory itself, or a block-less tail are errors, never panics or hangs.
#[test]
fn crafted_entries_under_unlink_and_rename() {
    let disk = hello();
    let table = le32(&disk.durable[0], 20);
    let docs = le32(&disk.durable[table], 64 + 8);
    let mut bad = crafted(disk.clone(), &[docs], 0, &0u32.to_le_bytes());
    let mut fs = mount(&mut bad).unwrap();
    let d = fs.lookup(ROOT, b"docs").unwrap();
    assert_eq!(fs.unlink(d, b"a.txt"), Err(Error::Corrupt));
    assert_eq!(fs.rename(d, b"a.txt", ROOT, b"x"), Err(Error::InvalidName));
    // `docs` holds itself: the search below it ends, and the move is allowed.
    let mut bad = crafted(disk.clone(), &[docs], 0, &1u32.to_le_bytes());
    let mut fs = mount(&mut bad).unwrap();
    let e = fs.mkdir(ROOT, b"e").unwrap();
    fs.rename(ROOT, b"docs", e, b"docs").unwrap();
    // A two-block directory whose second block is missing.
    let mut disk = MemDisk::new(64);
    let mut fs = format(&mut disk);
    let dir = fs.mkdir(ROOT, b"many").unwrap();
    for i in 0..74 {
        fs.create(dir, format!("{i}").as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    let table = le32(&disk.durable[0], 20);
    let mut bad = crafted(disk, &[table], 64 + 12, &0u32.to_le_bytes());
    let mut fs = mount(&mut bad).unwrap();
    assert_eq!(fs.unlink(dir, b"0"), Err(Error::Corrupt));
    assert_eq!(fs.rename(dir, b"0", ROOT, b"x"), Err(Error::Corrupt));
}

/// A free record pointing at a live block would release it when its inode is reused, and a later file would
/// overwrite `/docs/a.txt`'s data.
#[test]
fn crafted_free_record_cannot_alias_a_live_block() {
    let disk = hello();
    let table = le32(&disk.durable[0], 20);
    let a_data = le32(&disk.durable[table], 128 + 8) as u32;
    let mut disk = crafted(disk, &[table], 192 + 8, &a_data.to_le_bytes());
    // Rejected: mount falls back to the empty older slot.
    assert_eq!(snapshot(&mut disk.clone()), Ok(vec![]));
    let mut fs = mount(&mut disk).unwrap();
    fs.create(ROOT, b"x").unwrap();
    fs.commit().unwrap();
    fs.create(ROOT, b"y").unwrap();
    fs.commit().unwrap();
    for i in 0..10 {
        let f = fs.create(ROOT, format!("z{i}").as_bytes()).unwrap();
        fs.write(f, 0, b"EVIL!").unwrap();
    }
    fs.commit().unwrap();
    let mut fs = mount(&mut disk).unwrap();
    let a = fs
        .lookup(ROOT, b"docs")
        .and_then(|d| fs.lookup(d, b"a.txt"));
    let mut buf = [0; 5];
    let r = a.and_then(|a| fs.read(a, 0, &mut buf));
    assert!(r != Ok(5) || &buf == b"hello", "a.txt reads {buf:?}");
}

/// Mount falls back past a damaged newest table; its blocks stay reserved, so no write can reseal it as valid.
#[test]
fn fallback_keeps_the_damaged_newest_slot_reserved() {
    let mut disk = hello();
    let mut fs = mount(&mut disk).unwrap();
    for i in 0..8 {
        let f = fs.create(ROOT, format!("p{i}").as_bytes()).unwrap();
        fs.write(f, 0, &[1; 100]).unwrap();
    }
    fs.commit().unwrap();
    fs.create(ROOT, b"q").unwrap();
    fs.commit().unwrap();
    // Generation 4 is in slot 0.
    let t = le32(&disk.durable[0], 20);
    disk.durable[t][100] ^= 1;
    let old = snapshot(&mut disk.clone()).unwrap();
    let mut fs = mount(&mut disk).unwrap();
    let f = fs.create(ROOT, b"zeros").unwrap();
    fs.write(f, 0, &[0; MAX_FILE_SIZE as usize]).unwrap();
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(old));
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

/// Fails the next `.2` reads of block `.1`.
struct FailReads<'a>(&'a mut MemDisk, u64, usize);

impl Disk for FailReads<'_> {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        if block == self.1 && self.2 > 0 {
            self.2 -= 1;
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
    let mut fs = Fs::new(FailReads(&mut disk, 0, 1));
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
    // The newest slot's records load, then reading the older slot's table fails.
    let older_table = le32(&disk.durable[1], 20) as u64;
    let mut fs = Fs::new(FailReads(&mut disk, older_table, usize::MAX));
    assert_eq!(fs.mount(), Err(Error::Io));
    let docs = fs.lookup(ROOT, b"docs").unwrap();
    let a = fs.lookup(docs, b"a.txt").unwrap();
    assert_eq!(fs.write(a, 0, b"HE"), Err(Error::Io));
    assert_eq!(fs.commit(), Err(Error::Io));
    assert_eq!(snapshot(&mut disk.crash(|_, _| true)), Ok(hello_tree()));
}

/// Mounts `disk`, overwrites `/docs/a.txt`, adds `/src` and `/docs/b.txt`, moves `a.txt` and then `docs` into `/src`,
/// and commits.
fn change(disk: &mut MemDisk) -> Result<(), Error> {
    let mut fs = mount(disk)?;
    let docs = fs.lookup(ROOT, b"docs")?;
    let a = fs.lookup(docs, b"a.txt")?;
    fs.write(a, 0, b"HE")?;
    let src = fs.mkdir(ROOT, b"src")?;
    let b = fs.create(docs, b"b.txt")?;
    fs.write(b, 0, &[7; 5000])?;
    fs.rename(docs, b"a.txt", src, b"a.txt")?;
    fs.rename(ROOT, b"docs", src, b"docs")?;
    fs.commit()
}

/// Mounts `disk` after `change` and unlinks everything under `/src`, then commits.
fn remove(disk: &mut MemDisk) -> Result<(), Error> {
    let mut fs = mount(disk)?;
    let src = fs.lookup(ROOT, b"src")?;
    let docs = fs.lookup(src, b"docs")?;
    fs.unlink(src, b"a.txt")?;
    fs.unlink(docs, b"b.txt")?;
    fs.unlink(src, b"docs")?;
    fs.commit()
}

#[test]
fn power_cut_anywhere_leaves_the_old_or_the_new_state() {
    let mut changed = hello();
    change(&mut changed).unwrap();
    power_cut(hello(), change);
    power_cut(changed, remove);
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
    // `create` opens what is already there.
    assert_eq!(fs.create(ROOT, &long[..NAME_MAX]), Ok(f));
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

/// Counts the read, write and flush requests that reach `disk`.
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
        self.bump(0, 1);
        (&mut *self.0).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
        self.bump(1, 1);
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
    // [reads, writes, flushes]: both superblocks in one request, then each slot's inode table.
    assert_eq!(io.take(), [3, 0, 0]);
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

    fs.commit().unwrap();
    io.take();
    // Returning true stops before the next block; `start` begins at its entry's block.
    let mut seen = vec![];
    fs.readdir(dir, 0, |n, _, _| {
        seen.push(n.to_vec());
        true
    })
    .unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    fs.readdir(dir, 73, |n, _, kind| {
        seen.push(n.to_vec());
        assert_eq!(kind, Kind::File);
        false
    })
    .unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    assert_eq!(seen, [&b"0"[..], b"73", b"74"]);
    fs.readdir(dir, 75, |_, _, _| true).unwrap();
    assert_eq!(io.take(), [0, 0, 0]);
    // Creating an existing name scans once; truncating an empty file changes nothing.
    let zero = fs.create(dir, b"0").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
    fs.truncate(zero).unwrap();
    fs.commit().unwrap();
    assert_eq!(io.take(), [0, 0, 0]);

    // Within a directory: one scan, then the entry's block rewritten with the new name.
    fs.rename(docs, b"b.txt", docs, b"c.txt").unwrap();
    assert_eq!(io.take(), [1, 1, 0]);
    // The last entry moves into the freed slot of the buffered block, now fresh and rewritten in place.
    fs.unlink(docs, b"a.txt").unwrap();
    assert_eq!(io.take(), [0, 1, 0]);
    // Across directories: scans both, appends to the target; removing the last entry only shrinks the source.
    fs.rename(docs, b"c.txt", ROOT, b"c.txt").unwrap();
    assert_eq!(io.take(), [1, 1, 0]);
    // A directory move also reads every directory below it (both blocks of `many`), then the root block for its
    // last entry; it writes the root's block and a first block for the emptied `docs`.
    fs.rename(ROOT, b"many", docs, b"many").unwrap();
    assert_eq!(io.take(), [3, 2, 0]);
    // Unlinking the last entry only scans.
    fs.unlink(ROOT, b"c.txt").unwrap();
    assert_eq!(io.take(), [1, 0, 0]);
}
