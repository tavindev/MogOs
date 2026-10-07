use std::hint::black_box;
use std::time::Duration;

use cpu_time::ThreadTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use mogfs2::{BLOCK_SIZE, Buf, Disk, Error, Fs, Inode, ROOT, bitmap_words, cache_blocks};

#[path = "../../../benches/thread_time.rs"]
mod thread_time;

const FILES: usize = 400;
const POOL: usize = 64;

type Group<'a> = BenchmarkGroup<'a, thread_time::ThreadTime>;

struct MemDisk {
    blocks: Vec<Buf>,
}

impl MemDisk {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; BLOCK_SIZE]; blocks],
        }
    }
}

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        bufs.copy_from_slice(&self.blocks[block as usize..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        self.blocks[block as usize..][..bufs.len()].copy_from_slice(bufs);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.blocks.len() as u64
    }
}

struct Mem {
    cache: Vec<Buf>,
    bits: Vec<u64>,
}

impl Mem {
    fn new(blocks: usize) -> Self {
        Self {
            cache: vec![[0; BLOCK_SIZE]; cache_blocks(blocks as u64, POOL)],
            bits: vec![0; bitmap_words(blocks as u64)],
        }
    }
}

/// Thread CPU time of `iters` runs of `timed`, each on a freshly formatted `blocks`-block file system.
fn on_fresh(iters: u64, blocks: usize, mut timed: impl FnMut(&mut Fs<&mut MemDisk>)) -> Duration {
    (0..iters)
        .map(|_| {
            let mut disk = MemDisk::new(blocks);
            let mut mem = Mem::new(blocks);
            let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
            fs.format(1).unwrap();
            let start = ThreadTime::now();
            timed(&mut fs);
            start.elapsed()
        })
        .sum()
}

/// As v1's: create + 100-byte write + commit of `FILES` files in one directory, then lookups of each in the settled
/// directory, on a disk of `blocks` blocks. Each iteration is `FILES` ops: divide its time by `FILES` for ns per op.
fn small_files(g: &mut Group, blocks: usize) {
    let names: Vec<String> = (0..FILES).map(|i| format!("file-{i}")).collect();
    let create = |fs: &mut Fs<&mut MemDisk>| {
        for name in &names {
            let f = fs.create(ROOT, name.as_bytes()).unwrap();
            fs.write(f, 0, &[7; 100]).unwrap();
            fs.commit().unwrap();
        }
    };
    g.bench_function(format!("create+write+commit, {blocks} blocks"), |b| {
        b.iter_custom(|iters| on_fresh(iters, blocks, create))
    });
    let mut disk = MemDisk::new(blocks);
    let mut mem = Mem::new(blocks);
    let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
    fs.format(1).unwrap();
    create(&mut fs);
    g.bench_function(format!("lookup (400 entries), {blocks} blocks"), |b| {
        b.iter(|| {
            for name in &names {
                black_box(fs.lookup(ROOT, name.as_bytes()).unwrap());
            }
        })
    });
}

/// Lookups in a directory of 100k entries, in creation order; each iteration is the 100k lookups.
fn big_directory(g: &mut Group) {
    const N: usize = 100_000;
    let mut disk = MemDisk::new(16384);
    let mut mem = Mem::new(16384);
    let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
    fs.format(1).unwrap();
    let names: Vec<String> = (0..N).map(|i| format!("entry-{i:06}")).collect();
    for name in &names {
        fs.create(ROOT, name.as_bytes()).unwrap();
    }
    fs.commit().unwrap();
    g.bench_function("lookup (100k entries, 64-slot cache)", |b| {
        b.iter(|| {
            for name in &names {
                black_box(fs.lookup(ROOT, name.as_bytes()).unwrap());
            }
        })
    });
}

/// Reads the whole `size`-byte `file` sequentially, `out.len()` bytes at a time.
fn read_all(fs: &mut Fs<&mut MemDisk>, file: Inode, size: u64, out: &mut [u8]) {
    for at in (0..size).step_by(out.len()) {
        fs.read(file, at, out).unwrap();
    }
}

/// A 1 GiB file on the in-memory disk: mount and sequential read of a settled one, and sequential 1 MiB writes +
/// commit on a fresh disk.
fn big_file(g: &mut Group) {
    const SIZE: u64 = 1 << 30;
    let blocks = (SIZE / BLOCK_SIZE as u64) as usize + 16384;
    let buf = vec![7u8; 1 << 20];
    let mut out = vec![0u8; 1 << 20];
    let write = |fs: &mut Fs<&mut MemDisk>| {
        let f = fs.create(ROOT, b"big").unwrap();
        for at in (0..SIZE).step_by(buf.len()) {
            fs.write(f, at, &buf).unwrap();
        }
        fs.commit().unwrap();
        f
    };
    {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks);
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        fs.format(1).unwrap();
        let f = write(&mut fs);
        g.bench_function("mount, 1 GiB file", |b| b.iter(|| fs.mount().unwrap()));
        g.throughput(Throughput::Bytes(SIZE));
        g.bench_function("1 GiB sequential read", |b| {
            b.iter(|| read_all(&mut fs, f, SIZE, &mut out))
        });
    }
    g.bench_function("1 GiB sequential write + commit", |b| {
        b.iter_custom(|iters| {
            on_fresh(iters, blocks, |fs| {
                write(fs);
            })
        })
    });
}

/// Sequential read of a 64 MiB file written sequentially, and of one after random 4 KiB overwrites (16384, a whole
/// file's worth) and a commit: copy-on-write data fragments.
fn fragmentation(g: &mut Group) {
    const SIZE: u64 = 64 << 20;
    const PAGES: u64 = SIZE / BLOCK_SIZE as u64;
    let blocks = 4 * PAGES as usize;
    let mut out = vec![0u8; 1 << 20];
    g.throughput(Throughput::Bytes(SIZE));
    for (name, overwrite) in [
        ("64 MiB sequential read", false),
        (
            "64 MiB sequential read after 16384 random 4 KiB overwrites",
            true,
        ),
    ] {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks);
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        fs.format(1).unwrap();
        let f = fs.create(ROOT, b"f").unwrap();
        for at in (0..SIZE).step_by(out.len()) {
            fs.write(f, at, &out).unwrap();
        }
        fs.commit().unwrap();
        if overwrite {
            let mut rng = 1u64;
            for _ in 0..PAGES {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let at = rng % PAGES * BLOCK_SIZE as u64;
                fs.write(f, at, &[1; BLOCK_SIZE]).unwrap();
            }
            fs.commit().unwrap();
        }
        g.bench_function(name, |b| b.iter(|| read_all(&mut fs, f, SIZE, &mut out)));
    }
}

fn fs(c: &mut Criterion<thread_time::ThreadTime>) {
    let mut g = thread_time::group(c, "mogfs2");
    small_files(&mut g, 1024);
    small_files(&mut g, 16384);
    // Rows from here have costly setup (100k creates, a GiB of writes): criterion's minimum sample count.
    g.sample_size(10);
    big_directory(&mut g);
    big_file(&mut g);
    fragmentation(&mut g);
}

criterion_group! {
    name = benches;
    config = thread_time::config();
    targets = fs
}
criterion_main!(benches);
