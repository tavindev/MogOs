use std::hint::black_box;
use std::time::Instant;

use mogfs2::{BLOCK_SIZE, Buf, Disk, Error, Fs, ROOT, bitmap_words, cache_blocks};

const RUNS: usize = 51;
const FILES: usize = 400;
const POOL: usize = 64;

struct MemDisk {
    blocks: Vec<Buf>,
    requests: usize,
}

impl MemDisk {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; BLOCK_SIZE]; blocks],
            requests: 0,
        }
    }
}

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        self.requests += 1;
        bufs.copy_from_slice(&self.blocks[block as usize..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        self.requests += 1;
        self.blocks[block as usize..][..bufs.len()].copy_from_slice(bufs);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.requests += 1;
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

fn report(name: &str, unit: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{name}: min {:.1} {unit}, median {:.1} {unit} ({} runs)",
        samples[0],
        samples[samples.len() / 2],
        samples.len()
    );
}

/// As v1's: create + 100-byte write + commit of `FILES` files in one directory, then a lookup of each, on a disk of
/// `blocks` blocks.
fn small_files(blocks: usize) {
    let names: Vec<String> = (0..FILES).map(|i| format!("file-{i}")).collect();
    let (mut create, mut lookup) = (vec![], vec![]);
    for _ in 0..RUNS {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks);
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        fs.format(1).unwrap();
        let start = Instant::now();
        for name in &names {
            let f = fs.create(ROOT, name.as_bytes()).unwrap();
            fs.write(f, 0, &[7; 100]).unwrap();
            fs.commit().unwrap();
        }
        create.push(start.elapsed().as_nanos() as f64 / FILES as f64);
        let start = Instant::now();
        for name in &names {
            black_box(fs.lookup(ROOT, name.as_bytes()).unwrap());
        }
        lookup.push(start.elapsed().as_nanos() as f64 / FILES as f64);
    }
    report(
        &format!("mogfs2 create+write+commit, {blocks} blocks"),
        "ns/op",
        create,
    );
    report(
        &format!("mogfs2 lookup (400 entries), {blocks} blocks"),
        "ns/op",
        lookup,
    );
}

/// Lookups in a directory of 100k entries, in creation order.
fn big_directory() {
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
    let mut samples = vec![];
    for _ in 0..11 {
        let start = Instant::now();
        for name in &names {
            black_box(fs.lookup(ROOT, name.as_bytes()).unwrap());
        }
        samples.push(start.elapsed().as_nanos() as f64 / N as f64);
    }
    report(
        "mogfs2 lookup (100k entries, 64-slot cache)",
        "ns/op",
        samples,
    );
}

fn mib_per_s(bytes: u64, start: Instant) -> f64 {
    bytes as f64 / (1 << 20) as f64 / start.elapsed().as_secs_f64()
}

/// A 1 GiB file written sequentially in 1 MiB writes and committed, then read back, on the in-memory disk; then mount.
fn big_file() {
    const SIZE: u64 = 1 << 30;
    let blocks = (SIZE / BLOCK_SIZE as u64) as usize + 16384;
    let (mut write, mut read, mut mount, mut requests) = (vec![], vec![], vec![], 0);
    let buf = vec![7u8; 1 << 20];
    let mut out = vec![0u8; 1 << 20];
    for _ in 0..5 {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks);
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        fs.format(1).unwrap();
        let f = fs.create(ROOT, b"big").unwrap();
        let start = Instant::now();
        for at in (0..SIZE).step_by(buf.len()) {
            fs.write(f, at, &buf).unwrap();
        }
        fs.commit().unwrap();
        write.push(mib_per_s(SIZE, start));
        let start = Instant::now();
        for at in (0..SIZE).step_by(out.len()) {
            fs.read(f, at, &mut out).unwrap();
        }
        read.push(mib_per_s(SIZE, start));
        disk.requests = 0;
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        let start = Instant::now();
        fs.mount().unwrap();
        mount.push(start.elapsed().as_nanos() as f64);
        requests = disk.requests;
    }
    report("mogfs2 1 GiB sequential write + commit", "MiB/s", write);
    report("mogfs2 1 GiB sequential read", "MiB/s", read);
    report(
        &format!("mogfs2 mount, 1 GiB file ({requests} requests)"),
        "ns",
        mount,
    );
}

/// Sequential read of a 64 MiB file written sequentially, and again after random 4 KiB overwrites (16384, a whole
/// file's worth) and a commit: copy-on-write data fragments.
fn fragmentation() {
    const SIZE: u64 = 64 << 20;
    const PAGES: u64 = SIZE / BLOCK_SIZE as u64;
    let blocks = 4 * PAGES as usize;
    let (mut before, mut after) = (vec![], vec![]);
    let mut out = vec![0u8; 1 << 20];
    for run in 0..11u64 {
        let mut disk = MemDisk::new(blocks);
        let mut mem = Mem::new(blocks);
        let mut fs = Fs::new(&mut disk, &mut mem.cache, &mut mem.bits);
        fs.format(1).unwrap();
        let f = fs.create(ROOT, b"f").unwrap();
        for at in (0..SIZE).step_by(out.len()) {
            fs.write(f, at, &out).unwrap();
        }
        fs.commit().unwrap();
        let start = Instant::now();
        for at in (0..SIZE).step_by(out.len()) {
            fs.read(f, at, &mut out).unwrap();
        }
        before.push(mib_per_s(SIZE, start));
        let mut rng = run + 1;
        for _ in 0..PAGES {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let at = rng % PAGES * BLOCK_SIZE as u64;
            fs.write(f, at, &[1; BLOCK_SIZE]).unwrap();
        }
        fs.commit().unwrap();
        let start = Instant::now();
        for at in (0..SIZE).step_by(out.len()) {
            fs.read(f, at, &mut out).unwrap();
        }
        after.push(mib_per_s(SIZE, start));
    }
    report("mogfs2 64 MiB sequential read", "MiB/s", before);
    report(
        "mogfs2 64 MiB sequential read after 16384 random 4 KiB overwrites",
        "MiB/s",
        after,
    );
}

fn main() {
    let only = std::env::args().nth(1).filter(|a| !a.starts_with('-'));
    let run = |name: &str| only.as_deref().is_none_or(|o| name.contains(o));
    if run("small") {
        small_files(1024);
        small_files(16384);
    }
    if run("directory") {
        big_directory();
    }
    if run("file") {
        big_file();
    }
    if run("fragment") {
        fragmentation();
    }
}
