//! Networking: the stacks the board's net task polls, and the sockets processes reach through handles.
//!
//! - Up to three stacks: `ETH` on the NIC (with a `net=<ip>/<prefix>[,gw=<ip>]` bootarg), and the loopback pair `LO`
//!   (127.0.0.1: listeners and the connections they accept) and `PEER` (127.0.0.2: connections to 127/8 start here),
//!   joined by a `Wire`. A stack never sends to its own address, so loopback takes two.
//! - A socket is an entry of a fixed table reached by index and generation and counted by handles, like a pipe. Its
//!   TCP slot and rings come from a pool taken once when the network starts; `SOCKET_FRAMES` are charged to the
//!   creating process's budget for it (accounting: the memory is the pool's) and refunded with the last handle.
//! - Ops (receive, send, accept, connect) run in the submitter's context, where its buffers are mapped: each is tried
//!   when submitted (an accept only by `complete`, which makes its handle at once) and again by every `complete`
//!   (`io_wait`) until it finishes; the net task only polls the stacks. A socket holds at most one receive-side op
//!   (receive, accept, connect) and one send, so the ops a process has in flight are bounded by its sockets, which its
//!   budget bounds.

use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{Ipv4Addr, SocketAddrV4};

use net::{
    Config, Error, HalfOpen, MAX_FRAME, Mac, Neighbor, Nic, Proto, Socket, SocketId, Stack, State,
    Tcp, TcpId, TcpSocket, TimeWait,
};

use crate::handle::{CONNECT, LISTEN, Rights};
use crate::syscall::{
    EACCES, EADDRINUSE, EAGAIN, EBADF, EBUSY, ECONNREFUSED, ECONNRESET, EFAULT, EHOSTUNREACH,
    EINVAL, EISCONN, ENETUNREACH, ENFILE, ENOBUFS, ENOTCONN, EPIPE, ETIMEDOUT,
};
use crate::{Board, Scheduler};

const NEIGHBORS: usize = 8;
const UDP_SOCKETS: usize = 4;
/// Each UDP or ICMP socket's receive buffer (the `ETH` stack's, for `test=net`).
const UDP_BUFFER: usize = 4096;
/// TCP slots per stack, listeners included.
const SLOTS: usize = 16;
const HALF_OPEN: usize = 16;
const TIME_WAIT: usize = 8;
/// Each TCP slot's receive ring, and its send ring.
const RING: usize = 16 << 10;
const PAGE: usize = 4096;
/// What a socket charges its creator's budget: its two rings.
pub const SOCKET_FRAMES: usize = 2 * RING / PAGE;
const SOCKETS: usize = 32;
/// Connections a listener holds before they are accepted; one past it is reset. Each is charged to the listener's
/// owner until accepted, so a remote peer can make the kernel hold no connection nobody pays for.
pub const BACKLOG: usize = 8;
/// Frames each direction of the loopback wire holds: more than every slot's window in flight at once.
const WIRE: usize = 128;
const WIRE_FRAME: usize = 1536;
const _: () = assert!(MAX_FRAME <= WIRE_FRAME);
/// Rounds of polling `LO` then `PEER` per `poll`, so one call carries a segment and its answer.
const LOOPBACK_ROUNDS: usize = 4;

const ETH: usize = 0;
const LO: usize = 1;
const PEER: usize = 2;

/// Socket ops for `io_submit`.
pub const OP_RECEIVE: u64 = 0;
pub const OP_SEND: u64 = 1;
pub const OP_ACCEPT: u64 = 2;
/// `ptr` is the IPv4 address (as a big-endian `u32`), `len` the port.
pub const OP_CONNECT: u64 = 3;

/// A socket: its table index and the generation of that entry.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Sock {
    /// `u32` keeps copying an `Object` plain moves.
    pub index: u32,
    pub generation: u64,
}

/// A process: its scheduler slot and generation.
pub type Owner = (usize, u64);

/// The `net=` bootarg's address, prefix and gateway.
pub fn config(arg: &str) -> Option<Config> {
    let (addr, gateway) = match arg.split_once(",gw=") {
        Some((addr, gw)) => (addr, Some(gw.parse().ok()?)),
        None => (arg, None),
    };
    let (ip, prefix) = addr.split_once('/')?;
    let prefix: u32 = prefix.parse().ok().filter(|p| (1..=30).contains(p))?;
    Some(Config {
        ip: ip.parse().ok()?,
        netmask: Ipv4Addr::from(!0u32 << (32 - prefix)),
        gateway,
    })
}

/// Board memory `Network::new` needs, in frames.
pub fn frames(eth: bool) -> usize {
    let stacks = 2 + eth as usize;
    (stacks * SLOTS * 2 * RING + 2 * WIRE * WIRE_FRAME).div_ceil(PAGE)
}

/// The processes' budgets, by owner.
pub trait Budgets {
    /// Charges `frames` to `owner`; false, charging nothing, over its budget or once it has exited.
    fn charge(&mut self, owner: Owner, frames: usize) -> bool;
    /// Refunds `frames` to `owner`, if it still runs.
    fn refund(&mut self, owner: Owner, frames: usize);
    fn alive(&mut self, owner: Owner) -> bool;
}

impl<const N: usize> Budgets for Scheduler<N> {
    fn charge(&mut self, (slot, generation): Owner, frames: usize) -> bool {
        self.budget(slot, generation)
            .is_some_and(|b| b.charge(frames))
    }

    fn refund(&mut self, (slot, generation): Owner, frames: usize) {
        if let Some(budget) = self.budget(slot, generation) {
            budget.refund(frames);
        }
    }

    fn alive(&mut self, (slot, generation): Owner) -> bool {
        self.budget(slot, generation).is_some()
    }
}

/// Reads and writes the submitting process's memory.
pub trait UserMemory {
    fn bytes(&self, ptr: u64, len: usize) -> Option<&[u8]>;
    fn bytes_mut(&mut self, ptr: u64, len: usize) -> Option<&mut [u8]>;
}

#[derive(Clone, Copy)]
enum Kind {
    Receive { ptr: u64, len: usize },
    Send { ptr: u64, len: usize },
    Accept,
    Connect,
}

#[derive(Clone, Copy)]
struct Op {
    submitter: Owner,
    /// The rights of the handle it was submitted with: an accepted connection's handle gets no more.
    rights: Rights,
    tag: u64,
    kind: Kind,
    /// Its result, once finished and not yet reported.
    done: Option<i64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Conn {
    Fresh,
    /// A listener's slot on `ETH` (if up) and on `LO`.
    Listening([Option<TcpId>; 2]),
    /// A connection on stack `.0`.
    Open(usize, TcpId),
}

#[derive(Clone, Copy)]
struct Entry {
    generation: u64,
    /// 0 while the entry is free.
    handles: u32,
    owner: Owner,
    /// `CONNECT` and `LISTEN`, as the NetStack handle that made it held them.
    allowed: Rights,
    port: u16,
    conn: Conn,
    /// The receive-side op and the send.
    ops: [Option<Op>; 2],
    /// A listener's connections not yet accepted, each charged to `owner`.
    backlog: [Option<(usize, TcpId)>; BACKLOG],
}

const FREE: Entry = Entry {
    generation: 0,
    handles: 0,
    owner: (0, 0),
    allowed: 0,
    port: 0,
    conn: Conn::Fresh,
    ops: [None; 2],
    backlog: [None; BACKLOG],
};

/// What `complete` reports: an op's tag and result, or for an accept the connection as a new socket and the rights
/// its handle may have, which the caller makes and returns as the result (closing the socket if that fails).
pub struct Completion {
    pub tag: u64,
    pub result: i64,
    pub accepted: Option<(Sock, Rights)>,
}

/// Its parts live on the heap, so building it takes little stack.
pub struct Network {
    stacks: [Option<&'static mut Stack<'static>>; 3],
    wire: Wire,
    sockets: &'static mut [Entry],
}

impl Network {
    /// The loopback pair, and `ETH` at `eth`; their memory is `memory` (`frames(eth.is_some())` frames) and the heap
    /// (`None` if it is short). `key` seeds TCP's ISNs and ephemeral ports.
    pub fn new(eth: Option<Config>, key: [u64; 2], memory: &'static mut [u8]) -> Option<Self> {
        let (wire, mut rings) = memory.split_at_mut(2 * WIRE * WIRE_FRAME);
        let mut stack = |config, udp: &'static mut [Socket<'static>]| {
            let (mine, rest) = core::mem::take(&mut rings).split_at_mut(SLOTS * 2 * RING);
            rings = rest;
            let mut slots = Vec::new();
            slots.try_reserve_exact(SLOTS).ok()?;
            for pair in mine.as_chunks_mut::<{ 2 * RING }>().0 {
                let (rx, tx) = pair.split_at_mut(RING);
                slots.push(TcpSocket::new(rx, tx));
            }
            let tcp = Tcp::new(
                key,
                slots.leak(),
                leak(HALF_OPEN, || HalfOpen::EMPTY)?,
                leak(TIME_WAIT, || TimeWait::EMPTY)?,
            );
            let neighbors = leak(NEIGHBORS, || Neighbor::EMPTY)?;
            leak_one(Stack::new(config, neighbors, udp).with_tcp(tcp))
        };
        let eth = match eth {
            Some(config) => {
                let mut udp = Vec::new();
                udp.try_reserve_exact(UDP_SOCKETS).ok()?;
                for _ in 0..UDP_SOCKETS {
                    udp.push(Socket::new(leak(UDP_BUFFER, || 0)?));
                }
                Some(stack(config, udp.leak())?)
            }
            None => None,
        };
        let loopback = |ip| Config {
            ip,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: None,
        };
        let lo = stack(loopback(Ipv4Addr::LOCALHOST), &mut [])?;
        let peer = stack(loopback(Ipv4Addr::new(127, 0, 0, 2)), &mut [])?;
        Some(Network {
            stacks: [eth, Some(lo), Some(peer)],
            wire: Wire {
                frames: wire,
                len: [[0; WIRE]; 2],
                head: [0; 2],
                count: [0; 2],
            },
            sockets: leak(SOCKETS, || FREE)?,
        })
    }

    /// The stack on the NIC.
    pub fn eth(&mut self) -> Option<&mut Stack<'static>> {
        self.stacks[ETH].as_deref_mut()
    }

    /// Handles every received frame and due timer on every stack, then moves each listener's new connections into its
    /// backlog, charged to its owner (resetting any past `BACKLOG` or the budget); returns the next deadline and
    /// whether frames are still on the loopback wire (poll again).
    pub fn poll(
        &mut self,
        nic: Option<&mut impl Nic>,
        now: u64,
        budgets: &mut impl Budgets,
    ) -> (Option<u64>, bool) {
        let mut next = None;
        if let (Some(stack), Some(nic)) = (&mut self.stacks[ETH], nic) {
            next = stack.poll(nic, now);
        }
        for _ in 0..LOOPBACK_ROUNDS {
            for (side, s) in [LO, PEER].into_iter().enumerate() {
                let end = &mut WireEnd {
                    wire: &mut self.wire,
                    side,
                };
                let due = self.stacks[s]
                    .as_mut()
                    .and_then(|stack| stack.poll(end, now));
                next = earliest(next, due);
            }
            if self.wire.count == [0; 2] {
                break;
            }
        }
        for index in 0..SOCKETS {
            let Conn::Listening(ids) = self.sockets[index].conn else {
                continue;
            };
            for (id, s) in ids.into_iter().zip([ETH, LO]) {
                let Some(id) = id else { continue };
                while let Some(conn) = self.stack(s).accept(id) {
                    let entry = &mut self.sockets[index];
                    let free = entry.backlog.iter().position(Option::is_none);
                    match free.filter(|_| budgets.charge(entry.owner, SOCKET_FRAMES)) {
                        Some(i) => entry.backlog[i] = Some((s, conn)),
                        None => self.stack(s).abort(conn),
                    }
                }
            }
        }
        (next, self.wire.count != [0; 2])
    }

    /// A new socket owned by `owner`, charged `SOCKET_FRAMES`; `allowed` are the NetStack handle's rights
    /// (`CONNECT`, `LISTEN`). Its one handle is the caller's to make.
    pub fn socket(
        &mut self,
        owner: Owner,
        allowed: Rights,
        budgets: &mut impl Budgets,
    ) -> Result<Sock, i64> {
        let index = self.free()?;
        self.adopt(index, owner, allowed, Conn::Fresh, budgets)
    }

    /// Counts one more handle to `sock`.
    pub fn open(&mut self, sock: Sock) {
        if let Ok(entry) = self.entry(sock) {
            entry.handles += 1;
        }
    }

    /// Drops a handle to `sock`; the last one closes its connection, or its listener and resets its backlog (its ops
    /// are dropped), and refunds the owner if it still runs (a socket that outlives its owner is charged to nobody;
    /// the table bounds them).
    pub fn close(&mut self, sock: Sock, budgets: &mut impl Budgets) {
        let Ok(entry) = self.entry(sock) else {
            return;
        };
        entry.handles -= 1;
        if entry.handles > 0 {
            return;
        }
        // The whole entry goes, so nothing (a listener's slots, its backlog, ops) outlives the socket.
        let entry = core::mem::replace(
            entry,
            Entry {
                generation: entry.generation,
                ..FREE
            },
        );
        let mut frames = SOCKET_FRAMES;
        match entry.conn {
            Conn::Fresh => {}
            Conn::Listening(ids) => {
                for (id, s) in ids.into_iter().zip([ETH, LO]) {
                    if let Some(id) = id {
                        self.stack(s).tcp_close(id);
                    }
                }
                for (s, conn) in entry.backlog.into_iter().flatten() {
                    self.stack(s).abort(conn);
                    frames += SOCKET_FRAMES;
                }
            }
            Conn::Open(s, id) => self.stack(s).tcp_close(id),
        }
        budgets.refund(entry.owner, frames);
    }

    /// Sets the local port `listen` and `connect` use (0: an ephemeral one for `connect`).
    pub fn bind(&mut self, sock: Sock, port: u16) -> Result<(), i64> {
        let entry = self.entry(sock)?;
        if entry.conn != Conn::Fresh {
            return Err(EINVAL);
        }
        entry.port = port;
        Ok(())
    }

    /// Listens on the bound port on `ETH` (if up) and `LO`; `EADDRINUSE` if either has it.
    pub fn listen(&mut self, sock: Sock) -> Result<(), i64> {
        let entry = *self.entry(sock)?;
        if entry.allowed & LISTEN == 0 {
            return Err(EACCES);
        }
        if entry.conn != Conn::Fresh || entry.port == 0 {
            return Err(EINVAL);
        }
        let mut ids = [None; 2];
        for (id, s) in ids.iter_mut().zip([ETH, LO]) {
            let Some(stack) = &mut self.stacks[s] else {
                continue;
            };
            match stack.listen(entry.port) {
                Ok(listener) => *id = Some(listener),
                Err(error) => {
                    if let Some(listener) = ids[0] {
                        self.stack(ETH).tcp_close(listener);
                    }
                    return Err(errno(error));
                }
            }
        }
        self.entry(sock)?.conn = Conn::Listening(ids);
        Ok(())
    }

    /// Ends the send side of `sock`'s connection: a FIN follows the queued data.
    pub fn shutdown(&mut self, sock: Sock) -> Result<(), i64> {
        match self.entry(sock)?.conn {
            Conn::Open(s, id) => {
                self.stack(s).shutdown(id);
                Ok(())
            }
            _ => Err(ENOTCONN),
        }
    }

    /// Submits `op` with `tag` on `sock` for `submitter`, through a handle with `rights`; `ptr` and `len` are its
    /// buffer, checked to lie in user space (`OP_CONNECT`: the address and port). Tries it at once, except an accept.
    /// `EBUSY` while an op of the same side is in flight for a process that still runs.
    pub fn submit(
        &mut self,
        sock: Sock,
        (op, ptr, len, tag): (u64, u64, usize, u64),
        rights: Rights,
        (submitter, now): (Owner, u64),
        budgets: &mut impl Budgets,
        user: &mut impl UserMemory,
    ) -> Result<(), i64> {
        let kind = match op {
            OP_RECEIVE => Kind::Receive { ptr, len },
            OP_SEND => Kind::Send { ptr, len },
            OP_ACCEPT => Kind::Accept,
            OP_CONNECT => Kind::Connect,
            _ => return Err(EINVAL),
        };
        let side = (op == OP_SEND) as usize;
        let index = sock.index as usize;
        let entry = *self.entry(sock)?;
        if entry.ops[side].is_some_and(|o| budgets.alive(o.submitter)) {
            return Err(EBUSY);
        }
        if op == OP_CONNECT {
            let port = u16::try_from(len).map_err(|_| EINVAL)?;
            let ip = Ipv4Addr::from(u32::try_from(ptr).map_err(|_| EINVAL)?);
            if entry.allowed & CONNECT == 0 {
                return Err(EACCES);
            }
            if entry.conn != Conn::Fresh {
                return Err(EISCONN);
            }
            let s = if ip.is_loopback() { PEER } else { ETH };
            let stack = self.stacks[s].as_mut().ok_or(ENETUNREACH)?;
            let id = stack
                .connect(now, entry.port, SocketAddrV4::new(ip, port))
                .map_err(errno)?;
            self.sockets[index].conn = Conn::Open(s, id);
        }
        self.sockets[index].ops[side] = Some(Op {
            submitter,
            rights,
            tag,
            kind,
            done: None,
        });
        if op != OP_ACCEPT {
            let done = self.attempt(index, side, user);
            if let Some(op) = &mut self.sockets[index].ops[side] {
                op.done = done;
            }
        }
        Ok(())
    }

    /// Whether `owner` has an op in flight.
    pub fn in_flight(&self, owner: Owner) -> bool {
        self.sockets
            .iter()
            .filter(|e| e.handles > 0)
            .any(|e| e.ops.iter().flatten().any(|o| o.submitter == owner))
    }

    /// The next op of `owner` that finished, or finishes when tried again now. An accepted connection becomes a
    /// socket of `owner`: its charge moves from the listener's owner (`ENOBUFS`, the connection reset, if `owner`'s
    /// budget is short).
    pub fn complete(
        &mut self,
        owner: Owner,
        budgets: &mut impl Budgets,
        user: &mut impl UserMemory,
    ) -> Option<Completion> {
        for index in 0..SOCKETS {
            if self.sockets[index].handles == 0 {
                continue;
            }
            for side in 0..2 {
                let Some(op) = self.sockets[index].ops[side].filter(|o| o.submitter == owner)
                else {
                    continue;
                };
                let (result, accepted) = match (op.kind, op.done) {
                    (_, Some(done)) => (done, None),
                    (Kind::Accept, None) => match self.accept(index, owner, budgets) {
                        Some(Ok(sock)) => (0, Some((sock, op.rights))),
                        Some(Err(error)) => (error, None),
                        None => continue,
                    },
                    (_, None) => match self.attempt(index, side, user) {
                        Some(result) => (result, None),
                        None => continue,
                    },
                };
                self.sockets[index].ops[side] = None;
                return Some(Completion {
                    tag: op.tag,
                    result,
                    accepted,
                });
            }
        }
        None
    }

    /// The listener at `index`'s oldest queued connection as a new socket of `owner`, or why none can be; `None`
    /// while none is queued.
    fn accept(
        &mut self,
        index: usize,
        owner: Owner,
        budgets: &mut impl Budgets,
    ) -> Option<Result<Sock, i64>> {
        let entry = &mut self.sockets[index];
        if !matches!(entry.conn, Conn::Listening(_)) {
            return Some(Err(EINVAL));
        }
        let (s, id) = entry.backlog[0].take()?;
        entry.backlog.rotate_left(1);
        budgets.refund(entry.owner, SOCKET_FRAMES);
        let adopted = self
            .free()
            .and_then(|index| self.adopt(index, owner, 0, Conn::Open(s, id), budgets));
        if adopted.is_err() {
            self.stack(s).abort(id);
        }
        Some(adopted)
    }

    /// Tries op `side` of socket `index` (not an accept) once; its result if it finished.
    fn attempt(&mut self, index: usize, side: usize, user: &mut impl UserMemory) -> Option<i64> {
        let entry = self.sockets[index];
        let op = entry.ops[side]?;
        let Conn::Open(s, id) = entry.conn else {
            return Some(ENOTCONN);
        };
        let stack = self.stack(s);
        let result = match op.kind {
            Kind::Receive { ptr, len } => match user.bytes_mut(ptr, len) {
                Some(buf) => stack.recv(id, buf),
                None => return Some(EFAULT),
            },
            Kind::Send { ptr, len } => match user.bytes(ptr, len) {
                Some(data) => stack.send(id, data),
                None => return Some(EFAULT),
            },
            Kind::Connect => match stack.tcp_info(id) {
                Some(info) if info.error.is_some() => Err(info.error.unwrap()),
                Some(info) if matches!(info.state, State::SynSent | State::SynReceived) => {
                    Err(Error::WouldBlock)
                }
                _ => Ok(0),
            },
            Kind::Accept => Ok(0),
        };
        match result {
            Ok(n) => Some(n as i64),
            Err(Error::WouldBlock) => None,
            Err(Error::Closed) if matches!(op.kind, Kind::Send { .. }) => Some(EPIPE),
            Err(error) => Some(errno(error)),
        }
    }

    fn free(&self) -> Result<usize, i64> {
        self.sockets
            .iter()
            .position(|e| e.handles == 0)
            .ok_or(ENFILE)
    }

    /// Makes entry `index` a socket of `owner` with one handle, charging it.
    fn adopt(
        &mut self,
        index: usize,
        owner: Owner,
        allowed: Rights,
        conn: Conn,
        budgets: &mut impl Budgets,
    ) -> Result<Sock, i64> {
        if !budgets.charge(owner, SOCKET_FRAMES) {
            return Err(ENOBUFS);
        }
        let entry = &mut self.sockets[index];
        *entry = Entry {
            generation: entry.generation + 1,
            handles: 1,
            owner,
            allowed: allowed & (CONNECT | LISTEN),
            conn,
            ..FREE
        };
        Ok(Sock {
            index: index as u32,
            generation: entry.generation,
        })
    }

    fn stack(&mut self, s: usize) -> &mut Stack<'static> {
        self.stacks[s]
            .as_deref_mut()
            .expect("a socket's stack is up")
    }

    /// The live entry `sock` reaches.
    fn entry(&mut self, sock: Sock) -> Result<&mut Entry, i64> {
        match self.sockets.get_mut(sock.index as usize) {
            Some(e) if e.handles > 0 && e.generation == sock.generation => Ok(e),
            _ => Err(EBADF),
        }
    }
}

/// The musl errno for a stack error.
fn errno(error: Error) -> i64 {
    match error {
        Error::InUse => EADDRINUSE,
        Error::TableFull => ENFILE,
        Error::Closed => ENOTCONN,
        Error::Invalid | Error::TooBig => EINVAL,
        Error::NoRoute => ENETUNREACH,
        Error::Unresolved | Error::Busy | Error::WouldBlock => EAGAIN,
        Error::Reset => ECONNRESET,
        Error::Refused => ECONNREFUSED,
        Error::TimedOut => ETIMEDOUT,
        Error::Unreachable => EHOSTUNREACH,
    }
}

fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

/// `value` on the heap until power-off; `None` if the heap is short.
pub fn leak_one<T>(value: T) -> Option<&'static mut T> {
    let mut value = Some(value);
    leak(1, || value.take().unwrap()).map(|v| &mut v[0])
}

fn leak<T>(n: usize, value: impl FnMut() -> T) -> Option<&'static mut [T]> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.extend(core::iter::repeat_with(value).take(n));
    Some(v.leak())
}

/// The loopback link: two bounded queues of frames, queue `i` received by side `i`.
struct Wire {
    frames: &'static mut [u8],
    len: [[u16; WIRE]; 2],
    head: [usize; 2],
    count: [usize; 2],
}

/// Side `side` of the wire as a NIC: it receives queue `side` and sends into the other.
struct WireEnd<'w> {
    wire: &'w mut Wire,
    side: usize,
}

impl Nic for WireEnd<'_> {
    fn mac(&self) -> Mac {
        [2, 0, 0, 0, 0, 1 + self.side as u8]
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let (w, q) = (&mut *self.wire, 1 - self.side);
        if w.count[q] == WIRE || len > WIRE_FRAME {
            return false;
        }
        let i = (w.head[q] + w.count[q]) % WIRE;
        let at = (q * WIRE + i) * WIRE_FRAME;
        fill(&mut w.frames[at..at + len]);
        w.len[q][i] = len as u16;
        w.count[q] += 1;
        true
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        let (w, q) = (&mut *self.wire, self.side);
        if w.count[q] == 0 {
            return false;
        }
        let i = w.head[q];
        let at = (q * WIRE + i) * WIRE_FRAME;
        f(&w.frames[at..at + w.len[q][i] as usize]);
        w.head[q] = (i + 1) % WIRE;
        w.count[q] -= 1;
        true
    }
}

/// `test=net`: pings the gateway, sends `mog` to the UDP echo at the gateway's port `port` and prints both replies,
/// then the frame counters.
pub fn net_test<B: Board>(board: &mut B, gateway: Ipv4Addr, port: u16) {
    let ping = eth(board, |stack, _, _| stack.bind(Proto::Icmp, 1)).expect("bind");
    let request = [8, 0, 0, 0, 0, 0, 0, 1, b'm', b'o', b'g'];
    let (from, _) = round_trip(board, ping, SocketAddrV4::new(gateway, 0), &request);
    let _ = writeln!(board.console(), "ping: reply from {}", from.ip());
    let udp = eth(board, |stack, _, _| stack.bind(Proto::Udp, 7)).expect("bind");
    let (from, reply) = round_trip(board, udp, SocketAddrV4::new(gateway, port), b"mog");
    let reply = core::str::from_utf8(&reply).unwrap_or("?");
    let _ = writeln!(board.console(), "udp: echo {reply} from {from}");
    let c = eth(board, |stack, _, _| stack.counters);
    let dropped = c.malformed + c.checksum + c.fragments + c.ignored + c.no_socket + c.socket_full;
    let _ = writeln!(
        board.console(),
        "net: rx {} tx {} dropped {dropped} tx busy {}",
        c.rx,
        c.tx,
        c.tx_busy
    );
}

/// UDP round trips `test=bench-net` times, and datagrams it streams.
const BENCH_ROUND_TRIPS: u64 = 1000;
const BENCH_DATAGRAMS: u64 = 10_000;
/// Datagrams `test=bench-net` keeps in flight while streaming.
const BENCH_WINDOW: u64 = 16;

/// `test=bench-net`: 64-byte UDP datagrams to the echo at the gateway's port `port`: the round trip, the send cost of
/// a burst, and the cost per datagram with `BENCH_WINDOW` in flight each way.
pub fn net_bench<B: Board>(board: &mut B, gateway: Ipv4Addr, port: u16) {
    let to = SocketAddrV4::new(gateway, port);
    let udp = eth(board, |stack, _, _| stack.bind(Proto::Udp, 7)).expect("bind");
    let data = [0x5a; 64];
    round_trip(board, udp, to, &data);
    let start = board.uptime_us();
    for _ in 0..BENCH_ROUND_TRIPS {
        round_trip(board, udp, to, &data);
    }
    bench(board, "udp-rtt", start, BENCH_ROUND_TRIPS);
    let start = board.uptime_us();
    for _ in 0..BENCH_DATAGRAMS {
        send(board, udp, to, &data);
    }
    bench(board, "udp-tx", start, BENCH_DATAGRAMS);
    // Drop the burst's echoes before streaming.
    board.idle();
    board.run_others();
    while eth(board, |stack, _, _| stack.recv_from(udp, &mut [])).is_some() {}
    let start = board.uptime_us();
    let (mut sent, mut received) = (0, 0);
    while received < BENCH_DATAGRAMS {
        if sent < BENCH_DATAGRAMS && sent - received < BENCH_WINDOW {
            send(board, udp, to, &data);
            sent += 1;
        } else if eth(board, |stack, _, _| stack.recv_from(udp, &mut [])).is_some() {
            received += 1;
        } else {
            board.idle();
            board.run_others();
        }
    }
    bench(board, "udp-stream", start, BENCH_DATAGRAMS);
}

/// Runs `f` on the `ETH` stack and the NIC; after `start_net` with both.
fn eth<B: Board, R>(
    board: &mut B,
    f: impl FnOnce(&mut Stack<'static>, &mut B::Nic, u64) -> R,
) -> R {
    board.with_net(|network, nic, now| f(network.eth().expect("eth"), nic.expect("nic"), now))
}

fn bench<B: Board>(board: &mut B, name: &str, start: u64, n: u64) {
    let ns = (board.uptime_us() - start) * 1000 / n;
    let _ = writeln!(board.console(), "bench {name}: {ns} ns");
}

fn send<B: Board>(board: &mut B, socket: SocketId, to: SocketAddrV4, data: &[u8]) {
    while let Err(Error::Unresolved | Error::Busy) = eth(board, |stack, nic, now| {
        stack.send_to(nic, now, socket, to, data)
    }) {
        board.idle();
        board.run_others();
    }
}

/// Sends `data` from `socket` to `to`, retrying while the next hop resolves, and waits for the first reply.
fn round_trip<B: Board>(
    board: &mut B,
    socket: SocketId,
    to: SocketAddrV4,
    data: &[u8],
) -> (SocketAddrV4, Vec<u8>) {
    send(board, socket, to, data);
    let mut buf = [0; 64];
    loop {
        if let Some((from, n)) = eth(board, |stack, _, _| stack.recv_from(socket, &mut buf)) {
            return (from, buf[..n].to_vec());
        }
        board.idle();
        board.run_others();
    }
}
