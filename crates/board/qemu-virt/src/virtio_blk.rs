use core::hint::spin_loop;
use core::ops::Range;
use core::ptr;
use core::sync::atomic::{Ordering::SeqCst, fence};

use kernel::syscall::EIO;
use kernel::{BLOCK, Disk};
use mm::PhysAddr;

/// "virt", little-endian.
const MAGIC: u32 = 0x7472_6976;
const BLOCK_DEVICE: u32 = 2;

// virtio-mmio registers (virtio 1.2, 4.2.2).
const MAGIC_VALUE: usize = 0x000;
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

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
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
/// polled (no interrupt). Data moves through its own frame, so callers' buffers need not be physical memory.
pub struct VirtioBlk {
    base: PhysAddr,
    queue: PhysAddr,
    data: PhysAddr,
    /// Requests submitted so far, wrapping: the available ring's next index.
    idx: u16,
}

impl VirtioBlk {
    /// Sets up the block device at `base`, if there is one, with two frames from `alloc` (queue, data).
    ///
    /// # Safety
    /// `base` must be a virtio-mmio transport in device memory that nothing else drives, and `alloc`'s frames
    /// identity-mapped RAM that nothing else uses.
    pub unsafe fn new(
        base: PhysAddr,
        alloc: impl FnOnce() -> Option<Range<PhysAddr>>,
    ) -> Option<Self> {
        let mut disk = Self {
            base,
            queue: PhysAddr(0),
            data: PhysAddr(0),
            idx: 0,
        };
        if disk.reg(MAGIC_VALUE) != MAGIC
            || disk.reg(VERSION) != 2
            || disk.reg(DEVICE_ID) != BLOCK_DEVICE
        {
            return None;
        }
        disk.set(STATUS, 0);
        disk.set(STATUS, ACKNOWLEDGE);
        disk.set(STATUS, ACKNOWLEDGE | DRIVER);
        disk.set(DEVICE_FEATURES_SEL, 0);
        if disk.reg(DEVICE_FEATURES) & F_FLUSH == 0 {
            return None;
        }
        disk.set(DRIVER_FEATURES_SEL, 0);
        disk.set(DRIVER_FEATURES, F_FLUSH);
        disk.set(DRIVER_FEATURES_SEL, 1);
        disk.set(DRIVER_FEATURES, F_VERSION_1);
        disk.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        disk.set(QUEUE_SEL, 0);
        if disk.reg(STATUS) & FEATURES_OK == 0 || disk.reg(QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
            return None;
        }
        let frames = alloc()?;
        (disk.queue, disk.data) = (frames.start, PhysAddr(frames.start.0 + PAGE as u64));
        // SAFETY: the caller hands over the queue frame, which no reference aliases.
        unsafe { ptr::write_bytes(disk.queue.0 as *mut u8, 0, PAGE) };
        let data = disk.data.0;
        let queue = disk.queue();
        queue.avail.flags = AVAIL_NO_INTERRUPT;
        // Every request is this chain at descriptor 0, which the zeroed ring already names.
        queue.desc[0] = Desc {
            addr: &raw const queue.header as u64,
            len: size_of::<Header>() as u32,
            flags: DESC_NEXT,
            next: 1,
        };
        queue.desc[1] = Desc {
            addr: data,
            len: BLOCK as u32,
            flags: DESC_NEXT,
            next: 2,
        };
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
        disk.set(QUEUE_NUM, QUEUE_SIZE as u32);
        for (reg, addr) in areas {
            disk.set(reg, addr as u32);
            disk.set(reg + 4, (addr >> 32) as u32);
        }
        disk.set(QUEUE_READY, 1);
        disk.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        Some(disk)
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

    /// The data frame; the device reads or writes it only inside `request`.
    fn data(&mut self) -> &mut [u8; BLOCK] {
        // SAFETY: as in `queue`.
        unsafe { &mut *(self.data.0 as *mut [u8; BLOCK]) }
    }

    /// Submits a `kind` request for `block` (with the data frame, unless a flush) and polls until the device completes
    /// it.
    fn request(&mut self, kind: u32, block: u64) -> Result<(), i64> {
        let sector = block.checked_mul(BLOCK as u64 / SECTOR).ok_or(EIO)?;
        let idx = self.idx.wrapping_add(1);
        let queue = self.queue();
        queue.header = Header {
            kind,
            reserved: 0,
            sector,
        };
        queue.status = u8::MAX;
        queue.desc[0].next = if kind == T_FLUSH { 2 } else { 1 };
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
            _ => Err(EIO),
        }
    }
}

impl Disk for VirtioBlk {
    fn read(&mut self, block: u64, data: &mut [u8; BLOCK]) -> Result<(), i64> {
        self.request(T_IN, block)?;
        *data = *self.data();
        Ok(())
    }

    fn write(&mut self, block: u64, data: &[u8; BLOCK]) -> Result<(), i64> {
        *self.data() = *data;
        self.request(T_OUT, block)
    }

    fn flush(&mut self) -> Result<(), i64> {
        self.request(T_FLUSH, 0)
    }
}
