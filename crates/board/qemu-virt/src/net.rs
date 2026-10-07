//! The NIC and the stack the net task runs on it.

use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use arch::Lock;
use kernel::{Board, Event, Scheduler};
use mm::PhysAddr;
use net::Stack;

use crate::virtio_net::{POOL_FRAMES, VirtioNet};
use crate::{CPUS, GIC_DIST, KERNEL, MAX_TASKS, QemuVirt, VIRTIO, VIRTIO_COUNT, VIRTIO_STRIDE};

/// QEMU `virt` wires virtio-mmio transport `i` to SPI `16 + i`.
const VIRTIO_IRQ: u32 = 48;

/// Set by `start`. Lock order: `KERNEL`, then `NET`.
static NET: Lock<Option<(VirtioNet, Stack<'static>)>> = Lock::new(None);
/// The NIC's interrupt ID, `u32::MAX` without one.
pub static IRQ: AtomicU32 = AtomicU32::new(u32::MAX);
/// The net task has work: a frame arrived or a socket submitted.
static PENDING: AtomicBool = AtomicBool::new(false);
/// The stack's next deadline in ns, `u64::MAX` for none.
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

fn now() -> u64 {
    arch::uptime_us() * 1000
}

/// `Board::start_net`: sets up the first net device among the transports and routes its interrupt to core 0.
pub fn start(board: &mut QemuVirt, stack: Stack<'static>) -> bool {
    let alloc = || KERNEL.lock().frames.alloc();
    let pool = || Some(KERNEL.lock().frames.alloc_contiguous(POOL_FRAMES)?.start);
    // QEMU `virt` fills the transports from the highest address down with no gaps, as `Board::disk` relies on.
    let found = (0..VIRTIO_COUNT).rev().find_map(|i| {
        let base = PhysAddr(VIRTIO.0 + i * VIRTIO_STRIDE);
        // SAFETY: QEMU `virt`'s virtio-mmio transports, in the device-mapped GiB 0; `start` runs once, so nothing else
        // drives a net device; frames from the allocator are identity-mapped RAM nobody else uses.
        match unsafe { VirtioNet::new(base, alloc, pool) } {
            Ok(nic) => Some(Some((nic, i))),
            Err(0) => Some(None),
            Err(_) => None,
        }
    });
    let Some(Some((nic, index))) = found else {
        return false;
    };
    *NET.lock() = Some((nic, stack));
    let irq = VIRTIO_IRQ + index as u32;
    IRQ.store(irq, Relaxed);
    let dist = PhysAddr(GIC_DIST.load(Relaxed));
    if CPUS.load(Relaxed) > 1 {
        // SAFETY: the DTB's GICv2 distributor, in the device-mapped GiB 0; `irq` is an SPI, core 0's interface is 0.
        unsafe { arch::gic::route(dist, irq, 0) };
    }
    // SAFETY: as above.
    unsafe { arch::gic::unmask(dist, irq) };
    board.spawn(task, 0).expect("net task");
    board.start_timer();
    true
}

/// `Board::with_net`.
pub fn with<R>(f: impl FnOnce(&mut Stack<'static>, &mut VirtioNet, u64) -> R) -> R {
    let mut kernel = KERNEL.lock();
    let mut net = NET.lock();
    let (nic, stack) = net.as_mut().expect("with_net before start_net");
    let result = f(stack, nic, now());
    wake(&mut kernel.sched);
    result
}

/// The NIC's interrupt: acknowledges it and wakes the net task. Under `KERNEL`.
pub fn interrupt(sched: &mut Scheduler<MAX_TASKS>) {
    if let Some((nic, _)) = &mut *NET.lock() {
        nic.ack();
    }
    wake(sched);
}

/// A timer tick: wakes the net task once the stack's deadline passed. Under `KERNEL`.
pub fn tick(sched: &mut Scheduler<MAX_TASKS>) {
    if now() >= DEADLINE.load(Relaxed) {
        DEADLINE.store(u64::MAX, Relaxed);
        wake(sched);
    }
}

fn wake(sched: &mut Scheduler<MAX_TASKS>) {
    PENDING.store(true, Relaxed);
    sched.wake(Event::Net);
}

/// Polls the stack whenever there is work, then sleeps until the next wake.
fn task(_: &mut QemuVirt, _: usize) -> ! {
    loop {
        let mut kernel = KERNEL.lock();
        if !PENDING.swap(false, Relaxed) {
            kernel.sched.block(Event::Net);
            drop(kernel);
            arch::yield_now();
            continue;
        }
        if let Some((nic, stack)) = &mut *NET.lock() {
            let deadline = stack.poll(nic, now());
            DEADLINE.store(deadline.unwrap_or(u64::MAX), Relaxed);
        }
    }
}
