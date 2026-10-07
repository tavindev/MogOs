use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use mogfs::{BLOCK_SIZE, Disk, Error, Fs, ROOT};

#[path = "../../../benches/thread_time.rs"]
mod thread_time;
use thread_time::ThreadTime;

const FILES: usize = 400;

struct MemDisk(Vec<[u8; BLOCK_SIZE]>);

impl Disk for MemDisk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        bufs.copy_from_slice(&self.0[block as usize..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0[block as usize..][..bufs.len()].copy_from_slice(bufs);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.0.len() as u64
    }
}

fn formatted() -> Fs<MemDisk> {
    let mut fs = Fs::new(MemDisk(vec![[0; BLOCK_SIZE]; 1024]));
    fs.format().unwrap();
    fs
}

fn create(fs: &mut Fs<MemDisk>, names: &[String]) {
    for name in names {
        let f = fs.create(ROOT, name.as_bytes()).unwrap();
        fs.write(f, 0, &[7; 100]).unwrap();
        fs.commit().unwrap();
    }
}

/// On an in-memory disk: create + 100-byte write + commit of `FILES` files in one directory, then a lookup of each.
/// Each iteration is `FILES` ops: divide its time by `FILES` for ns per op.
fn fs(c: &mut Criterion<ThreadTime>) {
    let names: Vec<String> = (0..FILES).map(|i| format!("file-{i}")).collect();
    let mut g = thread_time::group(c, "mogfs");
    g.bench_function("create+write+commit", |b| {
        b.iter_batched_ref(formatted, |fs| create(fs, &names), BatchSize::PerIteration)
    });
    let mut fs = formatted();
    create(&mut fs, &names);
    g.bench_function("lookup", |b| {
        b.iter(|| {
            for name in &names {
                black_box(fs.lookup(ROOT, name.as_bytes()).unwrap());
            }
        })
    });
}

criterion_group! {
    name = benches;
    config = thread_time::config();
    targets = fs
}
criterion_main!(benches);
