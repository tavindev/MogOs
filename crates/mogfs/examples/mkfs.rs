//! Writes an empty MogFS image with a random name-hash seed:
//! `cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- disk.img [blocks]` (default 262144, a sparse
//! 1 GiB).

use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::os::unix::fs::FileExt;

use mogfs::{BLOCK_SIZE, Buf, Disk, Error, Fs, MAX_BLOCKS, MIN_POOL, bitmap_words, cache_blocks};

struct FileDisk(File, u64);

impl Disk for FileDisk {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        self.0
            .read_exact_at(bufs.as_flattened_mut(), block * BLOCK_SIZE as u64)
            .map_err(|_| Error::Io)
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        self.0
            .write_all_at(bufs.as_flattened(), block * BLOCK_SIZE as u64)
            .map_err(|_| Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.0.sync_data().map_err(|_| Error::Io)
    }

    fn blocks(&self) -> u64 {
        self.1
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: mkfs <image> [blocks]");
    let blocks = args.next().map_or(262144, |b| b.parse().expect("blocks"));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("create image");
    file.set_len(blocks * BLOCK_SIZE as u64)
        .expect("size image");
    let used = blocks.min(MAX_BLOCKS);
    let mut cache = vec![[0; BLOCK_SIZE]; cache_blocks(used, MIN_POOL)];
    let mut bits = vec![0; bitmap_words(used)];
    let mut fs = Fs::new(FileDisk(file, blocks), &mut cache, &mut bits);
    fs.format(RandomState::new().hash_one(0)).expect("format");
}
