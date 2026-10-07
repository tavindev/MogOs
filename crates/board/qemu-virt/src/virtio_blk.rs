use core::hint::spin_loop;
use core::ops::Range;
use core::ptr;
use core::sync::atomic::{Ordering::SeqCst, fence};

use kernel::{BLOCK_SIZE, Disk};
use mm::PhysAddr;
use mogfs::Error;

use crate::GIB;

const BLOCK_DEVICE: u32 = 2;

// virtio-mmio registers (virtio 1.2, 4.2.2).
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const STATUS: usize = 0x070;
const QUEUE_DESC: usize = 0x080;
const QUEUE_DRIVER: usize = 0x090;
const QUEUE_DEVICE: usize = 0x0a0;
/// Device configuration: virtio-blk's capacity in sectors, two 32-bit halves.
const CAPACITY: usize = 0x100;

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const FAILED: u32 = 128;
/// `VIRTIO_BLK_F_FLUSH`, feature word 0; QEMU keeps its write cache on only when it is negotiated.
const F_FLUSH: u32 = 1 << 9;
/// `VIRTIO_F_VERSION_1` (bit 32), feature word 1.
const F_VERSION_1: u32 = 1;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
/// A request takes at most three descriptors: header, data, status.
const QUEUE_SIZE: usize = 4;
const SECTOR: u64 = 512;
const PAGE: usize = 4096;

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [[u32; 2]; QUEUE_SIZE],
    avail_event: u16,
}

#[repr(C)]
struct Header {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// The queue's frame: the split virtqueue's three areas, then one request's header and status.
#[repr(C)]
struct Queue {
    desc: [Desc; QUEUE_SIZE],
    avail: Avail,
    used: Used,
    header: Header,
    status: u8,
}

const _: () = assert!(size_of::<Queue>() <= PAGE);

/// A virtio-blk device on a modern (version 2) virtio-mmio transport: one queue, one request at a time, completion
/// polled (no interrupt). The device moves data straight to and from the caller's blocks.
pub struct VirtioBlk {
    base: PhysAddr,
    queue: PhysAddr,
    blocks: u64,
    /// Requests submitted so far, wrapping: the available ring's next index.
    idx: u16,
}

impl VirtioBlk {
    /// Sets up the block device at `base`, if there is one, with its queue in a frame from `alloc`; marks a block
    /// device it cannot set up as failed. Otherwise returns the transport's device ID (0: empty).
    ///
    /// # Safety
    /// `base` must be a virtio-mmio transport in device memory that nothing else drives, and `alloc`'s frames
    /// identity-mapped RAM that nothing else uses.
    pub unsafe fn new(
        base: PhysAddr,
        alloc: impl FnOnce() -> Option<PhysAddr>,
    ) -> Result<Self, u32> {
        let mut disk = Self {
            base,
            queue: PhysAddr(0),
            blocks: 0,
            idx: 0,
        };
        // Device ID first, so an empty transport (ID 0) costs one read; the caller vouches for the magic value.
        let id = disk.reg(DEVICE_ID);
        if id != BLOCK_DEVICE || disk.reg(VERSION) != 2 {
            return Err(id);
        }
        // A device that reads status 0 is already reset, and the reset write costs QEMU about 20 us under hvf.
        if disk.reg(STATUS) != 0 {
            disk.set(STATUS, 0);
        }
        disk.set(STATUS, ACKNOWLEDGE | DRIVER);
        if disk.setup(alloc).is_none() {
            disk.set(STATUS, FAILED);
            return Err(id);
        }
        Ok(disk)
    }

    /// Negotiates features, reads the capacity and sets up the queue, up to `DRIVER_OK`.
    fn setup(&mut self, alloc: impl FnOnce() -> Option<PhysAddr>) -> Option<()> {
        self.set(DEVICE_FEATURES_SEL, 0);
        if self.reg(DEVICE_FEATURES) & F_FLUSH == 0 {
            return None;
        }
        self.set(DRIVER_FEATURES_SEL, 0);
        self.set(DRIVER_FEATURES, F_FLUSH);
        self.set(DRIVER_FEATURES_SEL, 1);
        self.set(DRIVER_FEATURES, F_VERSION_1);
        self.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        self.set(QUEUE_SEL, 0);
        if self.reg(STATUS) & FEATURES_OK == 0 || self.reg(QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
            return None;
        }
        let sectors = self.reg(CAPACITY) as u64 | (self.reg(CAPACITY + 4) as u64) << 32;
        self.blocks = sectors / (BLOCK_SIZE as u64 / SECTOR);
        self.queue = alloc()?;
        // SAFETY: the caller of `new` hands over the queue frame, which no reference aliases.
        unsafe { ptr::write_bytes(self.queue.0 as *mut u8, 0, PAGE) };
        let queue = self.queue();
        queue.avail.flags = AVAIL_NO_INTERRUPT;
        // Every request is this chain at descriptor 0, which the zeroed ring already names.
        queue.desc[0] = Desc {
            addr: &raw const queue.header as u64,
            len: size_of::<Header>() as u32,
            flags: DESC_NEXT,
            next: 1,
        };
        queue.desc[1].next = 2;
        queue.desc[2] = Desc {
            addr: &raw const queue.status as u64,
            len: 1,
            flags: DESC_WRITE,
            next: 0,
        };
        let areas = [
            (QUEUE_DESC, &raw const queue.desc as u64),
            (QUEUE_DRIVER, &raw const queue.avail as u64),
            (QUEUE_DEVICE, &raw const queue.used as u64),
        ];
        self.set(QUEUE_NUM, QUEUE_SIZE as u32);
        for (reg, addr) in areas {
            self.set(reg, addr as u32);
            self.set(reg + 4, (addr >> 32) as u32);
        }
        self.set(QUEUE_READY, 1);
        self.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        Some(())
    }

    fn reg(&self, offset: usize) -> u32 {
        // SAFETY: `base` is a virtio-mmio transport (`new`'s contract) and `offset` one of its registers.
        unsafe { ((self.base.0 as usize + offset) as *const u32).read_volatile() }
    }

    fn set(&mut self, offset: usize, value: u32) {
        // SAFETY: as in `reg`.
        unsafe { ((self.base.0 as usize + offset) as *mut u32).write_volatile(value) }
    }

    /// The queue frame; the device reads or writes it only inside `request`.
    fn queue(&mut self) -> &mut Queue {
        // SAFETY: a zeroed frame this driver owns (`new`'s contract), aligned and large enough for `Queue`.
        unsafe { &mut *(self.queue.0 as *mut Queue) }
    }

    /// Submits a `kind` request for the blocks from `block` on at the addresses `data` (empty for a flush, which has no
    /// data) and polls until the device completes it. An empty read or write does nothing (QEMU fails it); `Io` unless
    /// `data` is in the identity-mapped RAM GiB, where every kernel buffer lives at its physical address, past the last
    /// block, or if the device fails the request.
    fn request(&mut self, kind: u32, block: u64, data: Range<u64>) -> Result<(), Error> {
        match data.is_empty() {
            true if kind != T_FLUSH => return Ok(()),
            false if !(GIB <= data.start && data.end <= 2 * GIB) => return Err(Error::Io),
            _ => {}
        }
        let count = (data.end - data.start) / BLOCK_SIZE as u64;
        if block.checked_add(count).is_none_or(|end| end > self.blocks) {
            return Err(Error::Io);
        }
        let sector = block * (BLOCK_SIZE as u64 / SECTOR);
        let idx = self.idx.wrapping_add(1);
        let queue = self.queue();
        queue.header = Header {
            kind,
            reserved: 0,
            sector,
        };
        queue.status = u8::MAX;
        queue.desc[0].next = if data.is_empty() { 2 } else { 1 };
        queue.desc[1].addr = data.start;
        queue.desc[1].len = (data.end - data.start) as u32;
        queue.desc[1].flags = if kind == T_IN {
            DESC_NEXT | DESC_WRITE
        } else {
            DESC_NEXT
        };
        let (avail_idx, used_idx) = (&raw mut queue.avail.idx, &raw const queue.used.idx);
        // The device may read the available index at any time, so it must see the descriptors first.
        fence(SeqCst);
        // SAFETY: points into the queue frame.
        unsafe { avail_idx.write_volatile(idx) };
        fence(SeqCst);
        self.set(QUEUE_NOTIFY, 0);
        // SAFETY: as above; the device writes it, so each read must reach memory.
        while unsafe { used_idx.read_volatile() } != idx {
            spin_loop();
        }
        fence(SeqCst);
        self.idx = idx;
        match self.queue().status {
            0 => Ok(()),
            _ => Err(Error::Io),
        }
    }
}

impl Disk for VirtioBlk {
    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn read(&mut self, block: u64, data: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let start = data.as_mut_ptr() as u64;
        self.request(T_IN, block, start..start + size_of_val(data) as u64)
    }

    fn write(&mut self, block: u64, data: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let start = data.as_ptr() as u64;
        self.request(T_OUT, block, start..start + size_of_val(data) as u64)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.request(T_FLUSH, 0, 0..0)
    }
}
