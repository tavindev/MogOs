use kernel::file::{mkdir, open, readdir};
use kernel::handle::Object;
use kernel::syscall::{CREATE, EINVAL, ENAMETOOLONG, ENOENT};
use mogfs::{BLOCK_SIZE, Disk, Error, Fs, ROOT};

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

fn fs() -> Box<Fs<MemDisk>> {
    let mut fs = Box::new(Fs::new(MemDisk(vec![[0; BLOCK_SIZE]; 64])));
    fs.format().unwrap();
    fs
}

/// `readdir` into a buffer of `len` bytes from entry `start`, as text.
fn list(fs: &mut Fs<MemDisk>, start: u64, len: usize) -> Result<String, i64> {
    let mut out = vec![0; len];
    let n = readdir(fs, ROOT, start, &mut out)?;
    Ok(String::from_utf8(out[..n].to_vec()).unwrap())
}

#[test]
fn readdir_writes_whole_entries_and_never_overruns_a_tight_buffer() {
    let mut fs = fs();
    mkdir(&mut fs, ROOT, b"a").unwrap();
    open(&mut fs, ROOT, b"f", CREATE).unwrap();
    mkdir(&mut fs, ROOT, b"b").unwrap();
    // Every buffer size from too small for the first entry to room for all: whole entries only, each `/` kept.
    let expected = [
        (2, Err(EINVAL)),
        (3, Ok("a/\n")),
        (5, Ok("a/\n")),
        (6, Ok("a/\nf\n")),
        (8, Ok("a/\nf\n")),
        (9, Ok("a/\nf\nb/\n")),
        (64, Ok("a/\nf\nb/\n")),
    ];
    for (len, want) in expected {
        assert_eq!(list(&mut fs, 0, len).as_deref(), want.as_deref(), "{len}");
    }
    assert_eq!(list(&mut fs, 1, 64).as_deref(), Ok("f\nb/\n"));
    assert_eq!(list(&mut fs, 3, 64).as_deref(), Ok(""));
    assert_eq!(list(&mut fs, u64::MAX, 64).as_deref(), Ok(""));
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
