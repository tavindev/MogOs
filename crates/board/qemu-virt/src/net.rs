//! The NIC, the network the net task polls, and the socket calls on it.

use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use arch::Lock;
use kernel::handle::{DUPLICATE, Handles, MAX_HANDLES, Object, READ, TRANSFER, WRITE};
use kernel::network::{self, Network, Sock, UserMemory};
use kernel::syscall::{EINVAL, ENOBUFS, NetCall};
use kernel::{Board, Event};
use lock_order::{self as level, W};
use mm::PhysAddr;
use net::Config;

use crate::usermem::{UserIn, UserOut};
use crate::virtio_net::{NET_DEVICE, POOL_FRAMES, VirtioNet};
use crate::{
    GIC_DIST, KERNEL, MAX_PROCESSES, QemuVirt, Sched, VIRTIO, VIRTIO_COUNT, VIRTIO_STRIDE,
};

const _: () = assert!(MAX_PROCESSES <= network::MAX_HOLDERS);

/// QEMU `virt` wires virtio-mmio transport `i` to SPI `16 + i`.
const VIRTIO_IRQ: u32 = 48;

/// Set by the net task once it set the network up.
static NET: Lock<Option<(Option<VirtioNet>, &'static mut Network)>, level::Net> = Lock::new(None);
/// What `start` hands the net task to set up: the NIC's address, if any, and the TCP key.
static SETUP: Lock<Option<Setup>, level::Net> = Lock::new(None);
type Setup = (Option<Config>, [u64; 2]);
/// The NIC's interrupt ID, `u32::MAX` without one.
pub static IRQ: AtomicU32 = AtomicU32::new(u32::MAX);
/// The net task has work: a frame arrived, a socket call ran, or frames wait on the loopback wire.
static PENDING: AtomicBool = AtomicBool::new(false);
/// The next deadline in ns, `u64::MAX` for none.
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
/// `start` ran: boot-spawned processes get a NetStack handle, and the net task is not counted as a task.
pub static STARTED: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    arch::uptime_us() * 1000
}

/// Sets up the first net device among the transports. Holding no lock.
fn nic() -> Option<VirtioNet> {
    // SAFETY: the caller holds no lock, and each closure releases its lock before it returns.
    let alloc = || KERNEL.lock(&mut unsafe { arch::root() }).frames.alloc();
    let pool = || {
        // SAFETY: as above.
        let mut root = unsafe { arch::root() };
        Some(
            KERNEL
                .lock(&mut root)
                .frames
                .alloc_contiguous(POOL_FRAMES)?
                .start,
        )
    };
    // QEMU `virt` fills the transports from the highest address down with no gaps, as `Board::disk` relies on.
    let (nic, index) = (0..VIRTIO_COUNT).rev().find_map(|i| {
        let base = PhysAddr(VIRTIO.0 + i * VIRTIO_STRIDE);
        // SAFETY: QEMU `virt`'s virtio-mmio transports, in the device-mapped GiB 0; the net task's setup runs once, so
        // nothing else drives a net device; frames from the allocator are identity-mapped RAM nobody else uses.
        match unsafe { VirtioNet::new(base, alloc, pool) } {
            Ok(nic) => Some(Some((nic, i))),
            Err(0) => Some(None),
            Err(_) => None,
        }
    })??;
    IRQ.store(VIRTIO_IRQ + index as u32, Relaxed);
    Some(nic)
}

/// `Board::has_nic`: one device-ID read per transport, down to the first empty one.
pub fn present() -> bool {
    let id = |i| {
        let base = VIRTIO.0 + i * VIRTIO_STRIDE;
        // SAFETY: QEMU `virt`'s virtio-mmio transports, in the device-mapped GiB 0; reading the device ID changes nothing.
        unsafe { ((base + 8) as *const u32).read_volatile() }
    };
    // QEMU `virt` fills the transports from the highest address down with no gaps, as `Board::disk` relies on.
    (0..VIRTIO_COUNT)
        .rev()
        .map(id)
        .take_while(|&id| id != 0)
        .any(|id| id == NET_DEVICE)
}

/// `Board::start_net`: only spawns the net task, which does the setup.
pub fn start(board: &mut QemuVirt, config: Option<Config>, key: [u64; 2]) {
    // SAFETY: `Board::start_net`, which the kernel crate calls holding no lock.
    *SETUP.lock(&mut unsafe { arch::root() }) = Some((config, key));
    STARTED.store(true, Relaxed);
    board.spawn(task, 0).expect("net task");
}

/// Sets up the NIC (with an address), routing its interrupt to core 0, and the network, and starts the timer.
fn setup(board: &mut QemuVirt) {
    // SAFETY: the net task's start, holding no lock.
    let mut root = unsafe { arch::root() };
    let (eth, key) = SETUP.lock(&mut root).take().expect("net setup");
    let nic = eth.map(|_| nic().expect("a net device, as `present` found"));
    let frames = network::frames(eth.is_some());
    let range = KERNEL
        .lock(&mut root)
        .frames
        .alloc_contiguous(frames)
        .expect("net memory");
    // SAFETY: fresh identity-mapped frames that nothing else references, never freed.
    let memory =
        unsafe { core::slice::from_raw_parts_mut(range.start.0 as *mut u8, frames * 4096) };
    let network = Network::new(eth, key, memory)
        .and_then(network::leak_one)
        .expect("net heap");
    if nic.is_some() {
        let irq = IRQ.load(Relaxed);
        let dist = PhysAddr(GIC_DIST.load(Relaxed));
        // SAFETY: the DTB's GICv3 distributor, in the device-mapped GiB 0; `irq` is an SPI, routed to core 0.
        unsafe { arch::gic::route(dist, irq, crate::mpidr(0)) };
        // SAFETY: as above.
        unsafe { arch::gic::unmask(dist, irq) };
    }
    board.start_timer();
    // Last, so `with` returns only once everything is up.
    let mut kernel = KERNEL.lock(&mut root);
    *NET.lock(&mut kernel.parts().1) = Some((nic, network));
}

/// `Board::with_net`: first lets the net task finish its setup (boot context only).
pub fn with<R>(f: impl FnOnce(&mut Network, Option<&mut VirtioNet>, u64) -> R) -> R {
    // SAFETY: `Board::with_net`, which the kernel crate calls holding no lock.
    let mut root = unsafe { arch::root() };
    loop {
        let mut guard = KERNEL.lock(&mut root);
        let (kernel, mut w) = guard.parts();
        if NET.lock(&mut w).is_some() {
            break;
        }
        kernel.sched.block(arch::cpu(), Event::Idle);
        drop(guard);
        arch::yield_now();
    }
    let mut guard = KERNEL.lock(&mut root);
    let (kernel, mut w) = guard.parts();
    let mut net = NET.lock(&mut w);
    let (nic, network) = net.as_mut().expect("set up above");
    let result = f(network, nic.as_mut(), now());
    drop(net);
    wake(&mut kernel.sched);
    crate::kick(&mut kernel.sched, arch::cpu());
    result
}

/// Runs `f` on the network (started, since a socket handle exists) and wakes the net task. Under `KERNEL`.
fn net<R>(sched: &mut Sched, w: &mut W<'_, level::Kernel>, f: impl FnOnce(&mut Network) -> R) -> R {
    let result = f(NET
        .lock_masked(w)
        .as_mut()
        .expect("a socket without a network")
        .1);
    wake(sched);
    result
}

struct User;

impl UserMemory for User {
    fn read(&mut self, ptr: u64, dst: &mut [u8]) -> bool {
        UserIn::new(ptr, dst.len())
            .map(|user| user.read(0, dst))
            .is_some()
    }

    fn writable(&mut self, ptr: u64, len: usize) -> bool {
        UserOut::new(ptr, len).is_some()
    }

    fn write(&mut self, ptr: u64, src: &[u8]) -> bool {
        UserOut::new(ptr, src.len())
            .map(|user| user.write(0, src))
            .is_some()
    }
}

const SOCKET_RIGHTS: u64 = READ | WRITE | DUPLICATE | TRANSFER;

/// Runs a socket syscall for the current process: its result (an `io_wait`'s tag and peer in `out`, x1 and x2), or
/// `None` while `io_wait` must block. Out of line, so `board_syscall` stays as lean for every other call.
#[inline(never)]
pub fn syscall(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    cpu: usize,
    call: NetCall,
    out: &mut [u64; 2],
) -> Option<i64> {
    Some(match call {
        NetCall::Socket(allowed) => socket((sched, w), cpu, allowed),
        NetCall::Bind {
            sock,
            port,
            loopback,
        } => status(net(sched, w, |n| n.bind(sock, port, loopback))),
        NetCall::Listen { sock, backlog } => {
            let mut net = NET.lock_masked(w);
            let network = &mut net.as_mut().expect("a socket without a network").1;
            let result = network.listen(sock, backlog.into(), sched);
            drop(net);
            wake(sched);
            status(result)
        }
        NetCall::Shutdown(sock) => status(net(sched, w, |n| n.shutdown(sock))),
        NetCall::Submit {
            sock,
            op,
            rights,
            ptr,
            len,
            peer,
            tag,
        } => submit(
            (sched, w, cpu),
            sock,
            ((op.into(), ptr, len as usize, tag), peer),
            rights.into(),
        ),
        NetCall::IoWait => {
            let (result, done, peer) = io_wait((sched, w), cpu)?;
            (out[0], out[1]) = (done, peer);
            result
        }
    })
}

/// A socket of `cpu`'s current process with NetStack rights `allowed`, and its handle.
fn socket((sched, w): (&mut Sched, &mut W<'_, level::Kernel>), cpu: usize, allowed: u64) -> i64 {
    let mut net = NET.lock_masked(w);
    let network = &mut net.as_mut().expect("a NetStack without a network").1;
    let made = network.socket(sched.process(cpu), allowed, sched);
    drop(net);
    match made {
        Ok(sock) => handle((sched, w), cpu, sock, SOCKET_RIGHTS),
        Err(error) => error,
    }
}

/// A handle with `rights` to the new `sock` in `cpu`'s current process's table; closes it if the table is full.
fn handle(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    cpu: usize,
    sock: Sock,
    rights: u64,
) -> i64 {
    match sched.handles(cpu).insert(Object::Socket(sock), rights) {
        Ok(handle) => handle as i64,
        Err(error) => {
            let current = sched.process(cpu);
            close((sched, w), cpu, sock, current);
            error
        }
    }
}

/// `io_submit` of `op` on `sock` for `cpu`'s current process.
fn submit(
    (sched, w, cpu): (&mut Sched, &mut W<'_, level::Kernel>, usize),
    sock: Sock,
    (op, peer): ((u64, u64, usize, u64), (u32, u16)),
    rights: u64,
) -> i64 {
    let current = (sched.process(cpu), sched.generation(cpu));
    let mut net = NET.lock_masked(w);
    let network = &mut net.as_mut().expect("a socket without a network").1;
    let result = network.submit(sock, (op, peer), rights, (current, now()), sched, &mut User);
    drop(net);
    wake(sched);
    status(result)
}

/// `io_wait`'s result, tag and peer, or `None` while `cpu`'s current process's ops are all unfinished.
fn io_wait(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    cpu: usize,
) -> Option<(i64, u64, u64)> {
    let current = (sched.process(cpu), sched.generation(cpu));
    let mut net = NET.lock_masked(w);
    let Some((_, network)) = net.as_mut() else {
        return Some((EINVAL, 0, 0));
    };
    let Some(done) = network.complete(current, sched, &mut User) else {
        let waiting = network.in_flight(current);
        return (!waiting).then_some((EINVAL, 0, 0));
    };
    drop(net);
    // Receiving opened the window and sending queued data: either may owe a segment.
    wake(sched);
    let (result, peer) = match done.accepted {
        Some((sock, rights, peer)) => {
            let peer = u64::from(u32::from(*peer.ip())) << 16 | u64::from(peer.port());
            (handle((sched, w), cpu, sock, rights & SOCKET_RIGHTS), peer)
        }
        None => (done.result, 0),
    };
    Some((result, done.tag, peer))
}

/// Counts a new handle to `sock`.
pub fn open(sock: Sock, w: &mut W<'_, level::Kernel>) {
    if let Some((_, network)) = NET.lock_masked(w).as_mut() {
        network.open(sock);
    }
}

/// Drops a handle of process `holder` to `sock` on `cpu`; refunds `holder` once it has no other handle to it.
pub fn close(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    cpu: usize,
    sock: Sock,
    holder: usize,
) {
    // A process other than the current one is ending, its table already emptied.
    let last = holder != sched.process(cpu)
        || !sched
            .handles(cpu)
            .objects()
            .any(|o| o == Object::Socket(sock));
    if let Some((_, network)) = NET.lock_masked(w).as_mut() {
        network.close(sock, holder, last, sched);
    }
    wake(sched);
}

/// Before a `spawn`: the child's `budget` pays for each socket its table `child` reaches (`ENOBUFS`); `spawned`
/// records it after.
pub fn spawn_charge(
    child: &Handles,
    budget: &mut mm::Budget,
    w: &mut W<'_, level::Kernel>,
) -> Result<(), i64> {
    if !STARTED.load(Relaxed) {
        return Ok(());
    }
    let mut net = NET.lock_masked(w);
    let Some((_, network)) = net.as_mut() else {
        return Ok(());
    };
    let frames = sockets(child).map(|s| network.cost(s)).sum();
    budget.charge(frames).then_some(()).ok_or(ENOBUFS)
}

/// After a spawn `spawn_charge` allowed: the child at `index` holds its sockets, and the current process stops
/// paying for those it no longer holds.
pub fn spawned(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    cpu: usize,
    index: usize,
    child: &Handles,
) {
    if !STARTED.load(Relaxed) {
        return;
    }
    let mut net = NET.lock_masked(w);
    let Some((_, network)) = net.as_mut() else {
        return;
    };
    let current = sched.process(cpu);
    for sock in sockets(child) {
        network.hold(sock, index);
        if !sched
            .handles(cpu)
            .objects()
            .any(|o| o == Object::Socket(sock))
        {
            network.unhold(sock, current, sched);
        }
    }
}

/// The distinct sockets `handles` reaches.
fn sockets(handles: &Handles) -> impl Iterator<Item = Sock> {
    let mut found = [None; MAX_HANDLES];
    for (i, object) in handles.objects().enumerate() {
        if let Object::Socket(sock) = object
            && !found.contains(&Some(sock))
        {
            found[i] = Some(sock);
        }
    }
    found.into_iter().flatten()
}

fn status(result: Result<(), i64>) -> i64 {
    result.map_or_else(|error| error, |()| 0)
}

/// The NIC's interrupt: acknowledges it and wakes the net task. Under `KERNEL`.
pub fn interrupt(sched: &mut Sched, w: &mut W<'_, level::Kernel>) {
    if let Some((Some(nic), _)) = &mut *NET.lock_masked(w) {
        nic.ack();
    }
    wake(sched);
}

/// A timer tick: wakes the net task once the deadline passed. Under `KERNEL`.
pub fn tick(sched: &mut Sched) {
    if now() >= DEADLINE.load(Relaxed) {
        DEADLINE.store(u64::MAX, Relaxed);
        wake(sched);
    }
}

fn wake(sched: &mut Sched) {
    PENDING.store(true, Relaxed);
    sched.wake(Event::Net);
}

/// Sets the network up, then polls it whenever there is work and wakes every `io_wait`, and sleeps until the next
/// wake.
fn task(board: &mut QemuVirt, _: usize) -> ! {
    setup(board);
    // SAFETY: the net task's loop, holding no lock at its top.
    let mut root = unsafe { arch::root() };
    loop {
        let mut guard = KERNEL.lock(&mut root);
        let (kernel, mut w) = guard.parts();
        if !PENDING.swap(false, Relaxed) {
            kernel.sched.block(arch::cpu(), Event::Net);
            drop(guard);
            arch::yield_now();
            continue;
        }
        if let Some((nic, network)) = &mut *NET.lock(&mut w) {
            let (deadline, more) = network.poll(nic.as_mut(), now());
            DEADLINE.store(deadline.unwrap_or(u64::MAX), Relaxed);
            PENDING.fetch_or(more || nic.as_ref().is_some_and(VirtioNet::capped), Relaxed);
        }
        kernel.sched.wake(Event::NetIo);
        crate::kick(&mut kernel.sched, arch::cpu());
    }
}
