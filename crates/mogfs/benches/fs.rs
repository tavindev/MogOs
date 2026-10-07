use std::hint::black_box;
use std::time::Instant;

use mogfs::{BLOCK_SIZE, Disk, Error, Fs, ROOT};

const RUNS: usize = 51;
const FILES: usize = 400;

struct MemDisk(Vec<[u8; BLOCK_SIZE]>);

impl Disk for &mut MemDisk {
    fn read(&mut self, block: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
        *buf = self.0[block as usize];
        Ok(())
    }

    fn write(&mut self, block: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), Error> {
        self.0[block as usize] = *buf;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.0.len() as u64
    }
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{name}: min {:.1} ns/op, median {:.1} ns/op ({RUNS} runs x {FILES})",
        samples[0],
        samples[RUNS / 2]
    );
}

/// On an in-memory disk: create + 100-byte write + commit of `FILES` files in one directory, then a lookup of each.
fn main() {
    let names: Vec<String> = (0..FILES).map(|i| format!("file-{i}")).collect();
    let mut create = Vec::new();
    let mut lookup = Vec::new();
    for _ in 0..RUNS {
        let mut disk = MemDisk(vec![[0; BLOCK_SIZE]; 1024]);
        let mut fs = Fs::format(&mut disk).unwrap();
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
    report("mogfs create+write+commit", create);
    report("mogfs lookup", lookup);
}
