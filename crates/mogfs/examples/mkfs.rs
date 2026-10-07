//! Writes an empty MogFS image:
//! `cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- disk.img [blocks]` (default 16384, 64 MiB).

use std::fs::File;
use std::os::unix::fs::FileExt;

use mogfs::{BLOCK_SIZE, Disk, Error, Fs};

struct FileDisk(File, u64);

impl Disk for FileDisk {
    fn clamp(index: usize, len: usize) -> usize {
        index.min(len - 1)
    }

    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0
            .read_exact_at(bufs.as_flattened_mut(), block * BLOCK_SIZE as u64)
            .map_err(|_| Error::Io)
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
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
    let blocks = args.next().map_or(16384, |b| b.parse().expect("blocks"));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("create image");
    file.set_len(blocks * BLOCK_SIZE as u64)
        .expect("size image");
    let mut fs = Fs::new(FileDisk(file, blocks));
    fs.format().expect("format");
}
