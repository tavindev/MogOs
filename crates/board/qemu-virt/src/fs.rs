//! `KERNEL.fs`'s disk adapter over the virtio-blk device.

use kernel::{BLOCK_SIZE, Disk};
use mogfs::Error;

use crate::virtio_blk::VirtioBlk;

/// `KERNEL.fs`'s disk: `None` until `Board::mount` puts the device in, so the const `Fs::new` builds the static before
/// the device exists; `Io` while there is none.
pub(crate) struct FsDisk(pub(crate) Option<VirtioBlk>);

impl Disk for FsDisk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.flush()
    }

    fn blocks(&self) -> u64 {
        self.0.as_ref().map_or(0, VirtioBlk::blocks)
    }
}
