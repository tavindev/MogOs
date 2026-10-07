use core::ptr;
use core::sync::atomic::{Ordering::SeqCst, fence};

use mm::PhysAddr;
use net::{MAX_FRAME, Mac, Nic};

const NET_DEVICE: u32 = 1;

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
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC: usize = 0x080;
const QUEUE_DRIVER: usize = 0x090;
const QUEUE_DEVICE: usize = 0x0a0;
/// Device configuration: the MAC address.
const CONFIG_MAC: usize = 0x100;

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const FAILED: u32 = 128;
/// `VIRTIO_NET_F_MAC`, feature word 0.
const F_MAC: u32 = 1 << 5;
/// `VIRTIO_F_VERSION_1` (bit 32), feature word 1.
const F_VERSION_1: u32 = 1;

const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const RX: u32 = 0;
const TX: u32 = 1;
/// Descriptors per queue; each owns one buffer of the pool.
const QUEUE_SIZE: usize = 64;
/// `virtio_net_hdr` with `num_buffers`, which `VIRTIO_F_VERSION_1` always includes; no offloads, so it stays zero.
const HEADER: usize = 12;
/// One frame and its header, rounded up so two fit in a page.
const BUFFER: usize = 2048;
const _: () = assert!(HEADER + MAX_FRAME <= BUFFER);
const PAGE: usize = 4096;
/// The pool: `QUEUE_SIZE` receive buffers, then `QUEUE_SIZE` transmit buffers.
pub const POOL_FRAMES: usize = 2 * QUEUE_SIZE * BUFFER / PAGE;

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

/// A queue's frame: the split virtqueue's three areas.
#[repr(C)]
struct Queue {
    desc: [Desc; QUEUE_SIZE],
    avail: Avail,
    used: Used,
}

const _: () = assert!(size_of::<Queue>() <= PAGE);

/// A virtio-net device on a modern (version 2) virtio-mmio transport: one receive and one transmit queue, no
/// offloads. Every receive buffer stays posted, and a frame's buffer is re-posted once the stack has read it; the
/// interrupt fires for received frames only, and finished transmits are reclaimed when a buffer is needed.
pub struct VirtioNet {
    base: PhysAddr,
    mac: Mac,
    /// The receive and transmit queue frames.
    queues: [PhysAddr; 2],
    pool: PhysAddr,
    /// Used-ring entries consumed and available-ring entries posted so far, per queue (wrapping).
    seen: [u16; 2],
    posted: [u16; 2],
    /// Transmit descriptors not in flight, one bit each.
    tx_free: u64,
    /// Receive buffers re-posted since the last notify.
    reposted: usize,
    /// A poll stopped at `QUEUE_SIZE` frames with more waiting.
    capped: bool,
}

const _: () = assert!(QUEUE_SIZE <= 64);

impl VirtioNet {
    /// Sets up the net device at `base`, if there is one, with two queue frames from `alloc` and a pool of
    /// `POOL_FRAMES` contiguous frames from `alloc_pool`; marks a net device it cannot set up as failed. Otherwise
    /// returns the transport's device ID (0: empty); reading it is all it does to another device.
    ///
    /// # Safety
    /// `base` must be a virtio-mmio transport in device memory that nothing else drives as a net device, and the
    /// frames identity-mapped RAM that nothing else uses.
    pub unsafe fn new(
        base: PhysAddr,
        alloc: impl FnMut() -> Option<PhysAddr>,
        alloc_pool: impl FnOnce() -> Option<PhysAddr>,
    ) -> Result<Self, u32> {
        let mut nic = Self {
            base,
            mac: [0; 6],
            queues: [PhysAddr(0); 2],
            pool: PhysAddr(0),
            seen: [0; 2],
            posted: [QUEUE_SIZE as u16, 0],
            tx_free: u64::MAX >> (64 - QUEUE_SIZE),
            reposted: 0,
            capped: false,
        };
        let id = nic.reg(DEVICE_ID);
        if id != NET_DEVICE || nic.reg(VERSION) != 2 {
            return Err(id);
        }
        if nic.reg(STATUS) != 0 {
            nic.set(STATUS, 0);
        }
        nic.set(STATUS, ACKNOWLEDGE | DRIVER);
        if nic.setup(alloc, alloc_pool).is_none() {
            nic.set(STATUS, FAILED);
            return Err(id);
        }
        Ok(nic)
    }

    fn setup(
        &mut self,
        mut alloc: impl FnMut() -> Option<PhysAddr>,
        alloc_pool: impl FnOnce() -> Option<PhysAddr>,
    ) -> Option<()> {
        self.set(DEVICE_FEATURES_SEL, 0);
        if self.reg(DEVICE_FEATURES) & F_MAC == 0 {
            return None;
        }
        self.set(DRIVER_FEATURES_SEL, 0);
        self.set(DRIVER_FEATURES, F_MAC);
        self.set(DRIVER_FEATURES_SEL, 1);
        self.set(DRIVER_FEATURES, F_VERSION_1);
        self.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if self.reg(STATUS) & FEATURES_OK == 0 {
            return None;
        }
        let config = self.base.0 as usize + CONFIG_MAC;
        // SAFETY: the config space's MAC bytes, one byte-wide read each.
        self.mac = core::array::from_fn(|i| unsafe { ((config + i) as *const u8).read_volatile() });
        self.pool = alloc_pool()?;
        for q in [RX, TX] {
            self.set(QUEUE_SEL, q);
            if self.reg(QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            let frame = alloc()?;
            self.queues[q as usize] = frame;
            // SAFETY: the caller of `new` hands over the frame, which no reference aliases.
            unsafe { ptr::write_bytes(frame.0 as *mut u8, 0, PAGE) };
            let pool = self.pool.0 + (q as usize * QUEUE_SIZE * BUFFER) as u64;
            let queue = self.queue(q);
            for (i, desc) in queue.desc.iter_mut().enumerate() {
                desc.addr = pool + (i * BUFFER) as u64;
                desc.len = BUFFER as u32;
                desc.flags = if q == RX { DESC_WRITE } else { 0 };
            }
            if q == RX {
                for (i, slot) in queue.avail.ring.iter_mut().enumerate() {
                    *slot = i as u16;
                }
                queue.avail.idx = QUEUE_SIZE as u16;
            } else {
                queue.avail.flags = AVAIL_NO_INTERRUPT;
            }
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
        }
        self.set(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        self.set(QUEUE_NOTIFY, RX);
        Some(())
    }

    /// Whether the last poll stopped with frames still waiting.
    pub fn capped(&self) -> bool {
        self.capped
    }

    /// Acknowledges the device's interrupt, so its level drops.
    pub fn ack(&mut self) {
        let status = self.reg(INTERRUPT_STATUS);
        self.set(INTERRUPT_ACK, status);
    }

    fn reg(&self, offset: usize) -> u32 {
        // SAFETY: `base` is a virtio-mmio transport (`new`'s contract) and `offset` one of its registers.
        unsafe { ((self.base.0 as usize + offset) as *const u32).read_volatile() }
    }

    fn set(&mut self, offset: usize, value: u32) {
        // SAFETY: as in `reg`.
        unsafe { ((self.base.0 as usize + offset) as *mut u32).write_volatile(value) }
    }

    /// Queue `q`'s frame, which the device also reads and writes: the driver only touches entries the device has
    /// handed back, and every index the device wrote is read with a volatile load.
    fn queue(&mut self, q: u32) -> &mut Queue {
        // SAFETY: a zeroed frame this driver owns (`new`'s contract), aligned and large enough for `Queue`.
        unsafe { &mut *(self.queues[q as usize].0 as *mut Queue) }
    }

    /// The next used-ring entry of queue `q` the driver has not consumed, as (descriptor, length); the device writes
    /// both, so the caller range-checks them.
    fn next_used(&mut self, q: u32) -> Option<(usize, usize)> {
        let seen = self.seen[q as usize];
        let queue = self.queue(q);
        // SAFETY: points into the queue frame; the device writes it, so the read must reach memory.
        if unsafe { (&raw const queue.used.idx).read_volatile() } == seen {
            return None;
        }
        fence(SeqCst);
        let entry = &raw const queue.used.ring[seen as usize % QUEUE_SIZE];
        // SAFETY: as above.
        let [id, len] = unsafe { entry.read_volatile() };
        self.seen[q as usize] = seen.wrapping_add(1);
        Some((id as usize, len as usize))
    }

    /// The pool buffer of descriptor `id` of queue `q`; `id < QUEUE_SIZE`.
    fn buffer(&mut self, q: u32, id: usize) -> &mut [u8; BUFFER] {
        let addr = self.pool.0 as usize + (q as usize * QUEUE_SIZE + id) * BUFFER;
        // SAFETY: inside the pool this driver owns (`new`'s contract); the device does not touch a buffer between
        // handing it back and the driver posting it again.
        unsafe { &mut *(addr as *mut [u8; BUFFER]) }
    }

    /// Makes descriptor `id` of queue `q` available to the device.
    fn post(&mut self, q: u32, id: usize) {
        let idx = self.posted[q as usize];
        self.posted[q as usize] = idx.wrapping_add(1);
        let queue = self.queue(q);
        queue.avail.ring[idx as usize % QUEUE_SIZE] = id as u16;
        // The device may read the available index at any time, so it must see the entry first.
        fence(SeqCst);
        // SAFETY: points into the queue frame.
        unsafe { (&raw mut queue.avail.idx).write_volatile(idx.wrapping_add(1)) };
    }
}

impl Nic for VirtioNet {
    fn mac(&self) -> Mac {
        self.mac
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        while let Some((id, _)) = self.next_used(TX) {
            if id < QUEUE_SIZE {
                self.tx_free |= 1 << id;
            }
        }
        if self.tx_free == 0 || len > MAX_FRAME {
            return false;
        }
        let id = self.tx_free.trailing_zeros() as usize;
        self.tx_free &= !(1 << id);
        let buffer = self.buffer(TX, id);
        buffer[..HEADER].fill(0);
        fill(&mut buffer[HEADER..HEADER + len]);
        self.queue(TX).desc[id].len = (HEADER + len) as u32;
        self.post(TX, id);
        fence(SeqCst);
        self.set(QUEUE_NOTIFY, TX);
        true
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        // At most a ring's worth per poll, so a flood never holds the kernel lock without end.
        self.capped = self.reposted == QUEUE_SIZE;
        let used = if self.capped {
            None
        } else {
            self.next_used(RX)
        };
        let Some((id, len)) = used else {
            if core::mem::take(&mut self.reposted) > 0 {
                fence(SeqCst);
                self.set(QUEUE_NOTIFY, RX);
            }
            return false;
        };
        // A device-written id or length out of range drops the frame and leaks nothing but that descriptor.
        if id >= QUEUE_SIZE {
            return true;
        }
        if (HEADER..=BUFFER).contains(&len) {
            f(&self.buffer(RX, id)[HEADER..len]);
        }
        self.post(RX, id);
        self.reposted += 1;
        true
    }
}
