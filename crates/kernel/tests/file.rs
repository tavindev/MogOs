use kernel::file::{mkdir, open, readdir, rename, unlink};
use kernel::handle::Object;
use kernel::syscall::{
    CREATE, EBUSY, EEXIST, EINVAL, EISDIR, ENAMETOOLONG, ENOENT, ENOTEMPTY, TRUNC,
};
use mogfs::{BLOCK_SIZE, Disk, Error, Fs, MIN_POOL, ROOT, bitmap_words, cache_blocks};

struct MemDisk(Vec<[u8; BLOCK_SIZE]>);

impl Disk for MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let block = block as usize;
        bufs.copy_from_slice(&self.0[block..block + bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let block = block as usize;
        self.0[block..block + bufs.len()].copy_from_slice(bufs);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.0.len() as u64
    }
}

fn fs() -> Fs<'static, MemDisk> {
    let cache = vec![[0; BLOCK_SIZE]; cache_blocks(64, MIN_POOL)].leak();
    let bits = vec![0; bitmap_words(64)].leak();
    let mut fs = Fs::new(MemDisk(vec![[0; BLOCK_SIZE]; 64]), cache, bits);
    fs.format(1).unwrap();
    fs
}

/// `readdir` into a buffer of `len` bytes from `cursor`, as text, and the cursor after it.
fn list(fs: &mut Fs<MemDisk>, cursor: u64, len: usize) -> Result<(String, u64), i64> {
    let mut out = vec![0; len];
    let (n, next) = readdir(fs, ROOT, cursor, &mut out)?;
    Ok((String::from_utf8(out[..n].to_vec()).unwrap(), next))
}

#[test]
fn readdir_writes_whole_entries_and_never_overruns_a_tight_buffer() {
    let mut fs = fs();
    mkdir(&mut fs, ROOT, b"a").unwrap();
    open(&mut fs, ROOT, b"f", CREATE).unwrap();
    mkdir(&mut fs, ROOT, b"b").unwrap();
    let (all, end) = list(&mut fs, 0, 64).unwrap();
    assert_eq!(end, u64::MAX);
    let entries: Vec<&str> = all.split_inclusive('\n').collect();
    let mut sorted = entries.clone();
    sorted.sort();
    assert_eq!(sorted, ["a/\n", "b/\n", "f\n"]);
    // Every buffer size from too small for the first entry to room for all: whole entries only, each `/` kept.
    for len in 0..=all.len() {
        let fit = entries
            .iter()
            .scan(0, |n, e| {
                *n += e.len();
                Some(*n)
            })
            .take_while(|&n| n <= len)
            .last();
        let got = list(&mut fs, 0, len).map(|(text, _)| text);
        match fit {
            None => assert_eq!(got, Err(EINVAL), "{len}"),
            Some(n) => assert_eq!(got.as_deref(), Ok(&all[..n]), "{len}"),
        }
    }
    // Resuming from a cursor lists the rest, even after the entry before it is unlinked.
    let (first, cursor) = list(&mut fs, 0, entries[0].len()).unwrap();
    assert_eq!(first, entries[0]);
    let name = entries[0].trim_end_matches(['/', '\n']);
    unlink(&mut fs, ROOT, name.as_bytes(), |_| false).unwrap();
    assert_eq!(
        list(&mut fs, cursor, 64),
        Ok((entries[1..].concat(), u64::MAX))
    );
    assert_eq!(list(&mut fs, u64::MAX, 64), Ok((String::new(), u64::MAX)));
}

#[test]
fn paths_stay_below_their_directory_and_are_bounded() {
    let mut fs = fs();
    let docs = match mkdir(&mut fs, ROOT, b"docs").and(open(&mut fs, ROOT, b"docs", 0)) {
        Ok(Object::Dir(docs)) => docs,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        open(&mut fs, ROOT, b"docs/a", CREATE),
        Ok(Object::Node(_))
    ));
    assert!(matches!(open(&mut fs, docs, b"a", 0), Ok(Object::Node(_))));
    for path in [&b".."[..], b"../docs", b"/a", b"."] {
        assert_eq!(open(&mut fs, docs, path, 0), Err(EINVAL), "{path:?}");
    }
    for path in [
        &b"/docs"[..],
        b"docs//a",
        b"docs/./a",
        b"docs/../docs",
        b"docs/",
    ] {
        assert_eq!(open(&mut fs, ROOT, path, 0), Err(EINVAL), "{path:?}");
    }
    assert_eq!(open(&mut fs, ROOT, b"docs/b", 0), Err(ENOENT));
    // A path walks at most 16 components, so its lookups stay bounded even on a crafted, cyclic image.
    let deep = [&b"x/"[..]; 16].concat();
    assert_eq!(open(&mut fs, ROOT, &deep, 0), Err(ENAMETOOLONG));
    assert_eq!(mkdir(&mut fs, ROOT, &deep), Err(ENAMETOOLONG));
    assert_eq!(open(&mut fs, ROOT, &deep[..deep.len() - 2], 0), Err(ENOENT));
}

#[test]
fn create_opens_an_existing_directory_and_trunc_refuses_it() {
    let mut fs = fs();
    mkdir(&mut fs, ROOT, b"d").unwrap();
    assert!(matches!(
        open(&mut fs, ROOT, b"d", CREATE),
        Ok(Object::Dir(_))
    ));
    assert_eq!(open(&mut fs, ROOT, b"d", CREATE | TRUNC), Err(EISDIR));
    assert_eq!(open(&mut fs, ROOT, b"d", TRUNC), Err(EISDIR));
}

#[test]
fn unlink_and_rename_walk_paths_like_open() {
    let mut fs = fs();
    mkdir(&mut fs, ROOT, b"d").unwrap();
    open(&mut fs, ROOT, b"d/a", CREATE).unwrap();
    assert_eq!(unlink(&mut fs, ROOT, b"d", |_| false), Err(ENOTEMPTY));
    assert_eq!(
        rename(&mut fs, (ROOT, b"d/a"), (ROOT, b"d/../b")),
        Err(EINVAL)
    );
    rename(&mut fs, (ROOT, b"d/a"), (ROOT, b"b")).unwrap();
    open(&mut fs, ROOT, b"d/c", CREATE).unwrap();
    assert_eq!(rename(&mut fs, (ROOT, b"b"), (ROOT, b"d/c")), Err(EEXIST));
    assert_eq!(unlink(&mut fs, ROOT, b"d/a", |_| false), Err(ENOENT));
    unlink(&mut fs, ROOT, b"d/c", |_| false).unwrap();
    unlink(&mut fs, ROOT, b"d", |_| false).unwrap();
    assert_eq!(list(&mut fs, 0, 64), Ok(("b\n".into(), u64::MAX)));
    let deep = [&b"x/"[..]; 16].concat();
    assert_eq!(unlink(&mut fs, ROOT, &deep, |_| false), Err(ENAMETOOLONG));
    assert_eq!(
        rename(&mut fs, (ROOT, b"b"), (ROOT, &deep)),
        Err(ENAMETOOLONG)
    );
}

#[test]
fn unlink_refuses_an_inode_a_handle_reaches() {
    let mut fs = fs();
    let Ok(Object::Node(file)) = open(&mut fs, ROOT, b"f", CREATE) else {
        panic!()
    };
    fs.write(file, 0, b"mine").unwrap();
    assert_eq!(unlink(&mut fs, ROOT, b"f", |i| i == file), Err(EBUSY));
    open(&mut fs, ROOT, b"g", CREATE).unwrap();
    let mut buf = [0; 8];
    assert_eq!(fs.read(file, 0, &mut buf), Ok(4));
    assert_eq!(&buf[..4], b"mine");
}

#[test]
fn opens_count_dir_and_node_handles_until_the_last_closes() {
    use kernel::file::Opens;
    use kernel::handle::Object;
    use mogfs::ROOT;

    let mut opens = Opens::<4>::new();
    opens.open(Object::Console);
    assert!(!opens.held(ROOT));
    opens.open(Object::Dir(ROOT));
    opens.open(Object::Dir(ROOT));
    opens.close(Object::Dir(ROOT));
    assert!(opens.held(ROOT), "one left");
    opens.close(Object::Dir(ROOT));
    assert!(!opens.held(ROOT));
}
