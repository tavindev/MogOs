use std::hint::black_box;
use std::time::Instant;

use mogfs2::{BLOCK_SIZE, Block, Disk, Error, Fs, ROOT, bitmap_words, cache_blocks};

const RUNS: usize = 51;
const FILES: usize = 400;
const POOL: usize = 64;

struct MemDisk {
    blocks: Vec<Block>,
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
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        self.requests += 1;
        bufs.copy_from_slice(&self.blocks[block as usize..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
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
    cache: Vec<Block>,
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

/// As v1's: create + 100-byte write + commit of `FILES` files in one directory, then a lookup of each.
fn small_files() {
    let names: Vec<String> = (0..FILES).map(|i| format!("file-{i}")).collect();
    let (mut create, mut lookup) = (vec![], vec![]);
    for _ in 0..RUNS {
        let mut disk = MemDisk::new(1024);
        let mut mem = Mem::new(1024);
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
    report("mogfs2 create+write+commit", "ns/op", create);
    report("mogfs2 lookup (400 entries)", "ns/op", lookup);
}

fn main() {
    let only = std::env::args().nth(1).filter(|a| !a.starts_with('-'));
    let run = |name: &str| only.as_deref().is_none_or(|o| name.contains(o));
    if run("small") {
        small_files();
    }
}
