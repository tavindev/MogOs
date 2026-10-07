//! The NIC, the network the net task polls, and the socket calls on it.

use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use arch::Lock;
use kernel::handle::{DUPLICATE, Object, READ, TRANSFER, WRITE};
use kernel::network::{Network, SOCKET_FRAMES, Sock, UserMemory};
use kernel::syscall::{EINVAL, NetCall};
use kernel::{Board, Event, Scheduler};
use mm::PhysAddr;

use crate::usermem::{user_bytes, user_bytes_mut};
use crate::virtio_net::{POOL_FRAMES, VirtioNet};
use crate::{CPUS, GIC_DIST, KERNEL, MAX_TASKS, QemuVirt, VIRTIO, VIRTIO_COUNT, VIRTIO_STRIDE};

/// QEMU `virt` wires virtio-mmio transport `i` to SPI `16 + i`.
const VIRTIO_IRQ: u32 = 48;

/// Set by `start`. Lock order: `KERNEL`, then `NET`.
static NET: Lock<Option<(Option<VirtioNet>, &'static mut Network)>> = Lock::new(None);
/// The NIC's interrupt ID, `u32::MAX` without one.
pub static IRQ: AtomicU32 = AtomicU32::new(u32::MAX);
/// The net task has work: a frame arrived, a socket call ran, or frames wait on the loopback wire.
static PENDING: AtomicBool = AtomicBool::new(false);
/// The next deadline in ns, `u64::MAX` for none.
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
/// `start` ran: boot-spawned processes get a NetStack handle.
pub static STARTED: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    arch::uptime_us() * 1000
}

/// `Board::nic`: sets up the first net device among the transports.
pub fn nic() -> Option<VirtioNet> {
    let alloc = || KERNEL.lock().frames.alloc();
    let pool = || Some(KERNEL.lock().frames.alloc_contiguous(POOL_FRAMES)?.start);
    // QEMU `virt` fills the transports from the highest address down with no gaps, as `Board::disk` relies on.
    let (nic, index) = (0..VIRTIO_COUNT).rev().find_map(|i| {
        let base = PhysAddr(VIRTIO.0 + i * VIRTIO_STRIDE);
        // SAFETY: QEMU `virt`'s virtio-mmio transports, in the device-mapped GiB 0; `Board::nic` runs once, so nothing
        // else drives a net device; frames from the allocator are identity-mapped RAM nobody else uses.
        match unsafe { VirtioNet::new(base, alloc, pool) } {
            Ok(nic) => Some(Some((nic, i))),
            Err(0) => Some(None),
            Err(_) => None,
        }
    })??;
    IRQ.store(VIRTIO_IRQ + index as u32, Relaxed);
    Some(nic)
}

/// `Board::memory`.
pub fn memory(frames: usize) -> Option<&'static mut [u8]> {
    let range = KERNEL.lock().frames.alloc_contiguous(frames)?;
    // SAFETY: fresh identity-mapped frames that nothing else references, never freed.
    Some(unsafe { core::slice::from_raw_parts_mut(range.start.0 as *mut u8, frames * 4096) })
}

/// `Board::start_net`: routes the NIC's interrupt to core 0.
pub fn start(board: &mut QemuVirt, network: &'static mut Network, nic: Option<VirtioNet>) {
    let irq = nic.is_some().then(|| IRQ.load(Relaxed));
    *NET.lock() = Some((nic, network));
    STARTED.store(true, Relaxed);
    if let Some(irq) = irq {
        let dist = PhysAddr(GIC_DIST.load(Relaxed));
        if CPUS.load(Relaxed) > 1 {
            // SAFETY: the DTB's GICv2 distributor, in the device-mapped GiB 0; `irq` is an SPI, core 0's interface is 0.
            unsafe { arch::gic::route(dist, irq, 0) };
        }
        // SAFETY: as above.
        unsafe { arch::gic::unmask(dist, irq) };
    }
    board.spawn(task, 0).expect("net task");
    board.start_timer();
}

/// `Board::with_net`.
pub fn with<R>(f: impl FnOnce(&mut Network, Option<&mut VirtioNet>, u64) -> R) -> R {
    let mut kernel = KERNEL.lock();
    let mut net = NET.lock();
    let (nic, network) = net.as_mut().expect("with_net before start_net");
    let result = f(network, nic.as_mut(), now());
    wake(&mut kernel.sched);
    result
}

/// Runs `f` on the network (started, since a socket handle exists) and wakes the net task. Under `KERNEL`.
fn net<R>(sched: &mut Scheduler<MAX_TASKS>, f: impl FnOnce(&mut Network) -> R) -> R {
    let result = f(NET.lock().as_mut().expect("a socket without a network").1);
    wake(sched);
    result
}

struct User;

impl UserMemory for User {
    fn bytes(&self, ptr: u64, len: usize) -> Option<&[u8]> {
        user_bytes(ptr, len)
    }

    fn bytes_mut(&mut self, ptr: u64, len: usize) -> Option<&mut [u8]> {
        user_bytes_mut(ptr, len)
    }
}

const SOCKET_RIGHTS: u64 = READ | WRITE | DUPLICATE | TRANSFER;

/// Runs a socket syscall for the current process: its result (an `io_wait`'s tag in `tag`), or `None` while
/// `io_wait` must block. Out of line, so `board_syscall` stays as lean for every other call.
#[inline(never)]
pub fn syscall(sched: &mut Scheduler<MAX_TASKS>, call: NetCall, tag: &mut u64) -> Option<i64> {
    Some(match call {
        NetCall::Socket(allowed) => socket(sched, allowed),
        NetCall::Bind { sock, port } => status(net(sched, |n| n.bind(sock, port))),
        NetCall::Listen(sock) => status(net(sched, |n| n.listen(sock))),
        NetCall::Shutdown(sock) => status(net(sched, |n| n.shutdown(sock))),
        NetCall::Submit {
            sock,
            op,
            ptr,
            len,
            tag,
        } => submit(sched, sock, (op.into(), ptr, len as usize, tag)),
        NetCall::IoWait => {
            let (result, done) = io_wait(sched)?;
            *tag = done;
            result
        }
    })
}

/// A socket of the current process with NetStack rights `allowed`, and its handle.
fn socket(sched: &mut Scheduler<MAX_TASKS>, allowed: u64) -> i64 {
    let owner = (sched.current().0, sched.generation());
    let mut net = NET.lock();
    let network = &mut net.as_mut().expect("a NetStack without a network").1;
    let made = network.socket(owner, allowed, &mut sched.memory().budget);
    drop(net);
    match made {
        Ok(sock) => handle(sched, sock),
        Err(error) => error,
    }
}

/// A handle to the new `sock` in the current process's table; closes it if the table is full.
fn handle(sched: &mut Scheduler<MAX_TASKS>, sock: Sock) -> i64 {
    match sched.handles().insert(Object::Socket(sock), SOCKET_RIGHTS) {
        Ok(handle) => handle as i64,
        Err(error) => {
            close(sched, sock);
            error
        }
    }
}

/// `io_submit` of `op` on `sock` for the current process.
fn submit(sched: &mut Scheduler<MAX_TASKS>, sock: Sock, op: (u64, u64, usize, u64)) -> i64 {
    let current = (sched.current().0, sched.generation());
    let mut net = NET.lock();
    let network = &mut net.as_mut().expect("a socket without a network").1;
    let alive = |(slot, generation)| sched.budget(slot, generation).is_some();
    let result = network.submit(sock, op, (current, now()), alive, &mut User);
    drop(net);
    wake(sched);
    status(result)
}

/// `io_wait`'s result and tag, or `None` while the current process's ops are all unfinished.
fn io_wait(sched: &mut Scheduler<MAX_TASKS>) -> Option<(i64, u64)> {
    let current = (sched.current().0, sched.generation());
    let mut net = NET.lock();
    let Some((_, network)) = net.as_mut() else {
        return Some((EINVAL, 0));
    };
    let Some(done) = network.complete(current, &mut sched.memory().budget, &mut User) else {
        let waiting = network.in_flight(current);
        return (!waiting).then_some((EINVAL, 0));
    };
    drop(net);
    // Receiving opened the window and sending queued data: either may owe a segment.
    wake(sched);
    let result = match done.accepted {
        Some(sock) => handle(sched, sock),
        None => done.result,
    };
    Some((result, done.tag))
}

/// Counts a new handle to `sock`.
pub fn open(sock: Sock) {
    if let Some((_, network)) = NET.lock().as_mut() {
        network.open(sock);
    }
}

/// Drops a handle to `sock`; the last one refunds its owner, if it still runs.
pub fn close(sched: &mut Scheduler<MAX_TASKS>, sock: Sock) {
    if let Some((slot, generation)) = net(sched, |n| n.close(sock))
        && let Some(budget) = sched.budget(slot, generation)
    {
        budget.refund(SOCKET_FRAMES);
    }
}

fn status(result: Result<(), i64>) -> i64 {
    result.map_or_else(|error| error, |()| 0)
}

/// The NIC's interrupt: acknowledges it and wakes the net task. Under `KERNEL`.
pub fn interrupt(sched: &mut Scheduler<MAX_TASKS>) {
    if let Some((Some(nic), _)) = &mut *NET.lock() {
        nic.ack();
    }
    wake(sched);
}

/// A timer tick: wakes the net task once the deadline passed. Under `KERNEL`.
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

/// Polls the network whenever there is work and wakes every `io_wait`, then sleeps until the next wake.
fn task(_: &mut QemuVirt, _: usize) -> ! {
    loop {
        let mut kernel = KERNEL.lock();
        if !PENDING.swap(false, Relaxed) {
            kernel.sched.block(Event::Net);
            drop(kernel);
            arch::yield_now();
            continue;
        }
        if let Some((nic, network)) = &mut *NET.lock() {
            let (deadline, more) = network.poll(nic.as_mut(), now());
            DEADLINE.store(deadline.unwrap_or(u64::MAX), Relaxed);
            PENDING.fetch_or(more, Relaxed);
        }
        kernel.sched.wake(Event::NetIo);
    }
}
