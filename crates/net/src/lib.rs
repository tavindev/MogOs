//! The network stack: Ethernet, ARP, IPv4, ICMP echo, UDP and TCP (`tcp.rs`) over a `Nic`, in caller-supplied memory.
//!
//! - No clock: every entry point takes `now` in ns, and `poll` returns the next deadline, so a seed replays a run.
//! - Every received frame is untrusted: each field is range-checked once when decoded, and a frame that fails is
//!   dropped and counted in `Counters`, never a panic. IPv4 fragments are dropped and counted (no reassembly).
//! - ARP learns only from replies to our own requests whose sender MAC matches the Ethernet source; a request
//!   aimed at us is answered but never learned. A full cache evicts its least recently used entry.
//! - Sockets are UDP ports and ICMP echo identifiers (a Linux ping socket: the caller sends and receives whole echo
//!   messages, the stack sets the identifier and checksum). Each socket queues received datagrams in its own buffer
//!   as records: length u16, source address u32, source port u16 (0 for ICMP), data.
//! - Checksums are summed over the payload as it is copied: into the NIC's buffer on send, into the socket buffer
//!   on receive (a datagram counts only once its checksum is good).
#![cfg_attr(not(test), no_std)]

use core::net::{Ipv4Addr, SocketAddrV4};

mod tcp;
pub use tcp::{HalfOpen, State, Tcp, TcpId, TcpInfo, TcpSocket, TimeWait};

pub type Mac = [u8; 6];

/// Largest frame the stack sends: an Ethernet header and a 1500-byte IPv4 packet.
pub const MAX_FRAME: usize = ETH + MAX_PACKET;

const MAX_PACKET: usize = 1500;
const BROADCAST: Mac = [0xff; 6];
const ETH: usize = 14;
const IP: usize = 20;
const UDP: usize = 8;
const ICMP: usize = 8;
const ARP: usize = 28;
const TYPE_ARP: [u8; 2] = [8, 6];
const TYPE_IPV4: [u8; 2] = [8, 0];
const PROTO_ICMP: u8 = 1;
const PROTO_UDP: u8 = 17;
const ECHO_REPLY: u8 = 0;
const ECHO_REQUEST: u8 = 8;
const DEST_UNREACHABLE: u8 = 3;
const TIME_EXCEEDED: u8 = 11;
const SEC: u64 = 1_000_000_000;
const ARP_RETRY: u64 = SEC;
const ARP_TRIES: u8 = 3;
/// A neighbour learned this long ago is asked again, while still in use.
const ARP_STALE: u64 = 60 * SEC;
const RECORD: usize = 8;

/// A network device. Frames carry no FCS; the device pads short frames.
pub trait Nic {
    fn mac(&self) -> Mac;
    /// Largest IPv4 packet, header included.
    fn mtu(&self) -> usize;
    /// Calls `fill` with a `len`-byte transmit buffer; false, without calling it, when none is free.
    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool;
    /// Calls `f` with the next received frame; false when there is none.
    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The port (or ICMP identifier) is bound.
    InUse,
    /// Every socket is bound.
    TableFull,
    /// The socket is not bound.
    Closed,
    /// Port 0, or ICMP data that is not an echo request.
    Invalid,
    /// The datagram does not fit in one packet.
    TooBig,
    NoRoute,
    /// The next hop's MAC is not known yet; an ARP request is out, so retry after a `poll`.
    Unresolved,
    /// The NIC has no free transmit buffer.
    Busy,
    /// Nothing to receive yet, or no room to send.
    WouldBlock,
    /// The peer reset the connection.
    Reset,
    /// The peer refused the connection (a RST answered our SYN).
    Refused,
    /// The peer stopped acknowledging.
    TimedOut,
    /// An ICMP hard error answered our SYN.
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Udp,
    Icmp,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Option<Ipv4Addr>,
}

/// Frames received and sent, and why each dropped frame was dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub rx: u64,
    pub tx: u64,
    /// A truncated or inconsistent header, or a field out of range.
    pub malformed: u64,
    pub checksum: u64,
    pub fragments: u64,
    /// Well formed, but not for us or not handled: another host's address, an unsolicited ARP reply, a protocol.
    pub ignored: u64,
    pub no_socket: u64,
    pub socket_full: u64,
    /// Frames not sent because the NIC had no free buffer.
    pub tx_busy: u64,
    /// TCP segments, and ICMP errors about a connection, that TCP accepted.
    pub tcp: u64,
    /// TCP segments (and ICMP errors) that failed a sequence, acknowledgment or state check.
    pub unacceptable: u64,
    /// Challenge ACKs sent (RFC 5961), at most `CHALLENGES` per second per connection.
    pub challenge_acks: u64,
    /// SYN-ACKs sent as SYN cookies, because the half-open table was full.
    pub syn_cookies: u64,
    /// ACKs to a listener whose cookie failed: forged, for another SYN or expired.
    pub bad_cookies: u64,
    /// TIME_WAIT entries reused before they expired because the table was full.
    pub time_wait_reused: u64,
}

#[derive(Clone, Copy)]
enum Reason {
    Malformed,
    Checksum,
    Fragment,
    Ignored,
    NoSocket,
    SocketFull,
    Unacceptable,
}

/// An ARP cache entry; the caller provides the table, filled with `Neighbor::EMPTY`.
#[derive(Clone, Copy)]
pub struct Neighbor {
    /// Unspecified when the entry is free.
    ip: Ipv4Addr,
    mac: Option<Mac>,
    /// When the MAC was learned, or while `tries > 0`, when the last of `tries` requests went out.
    at: u64,
    tries: u8,
    used: u64,
}

impl Neighbor {
    pub const EMPTY: Self = Neighbor {
        ip: Ipv4Addr::UNSPECIFIED,
        mac: None,
        at: 0,
        tries: 0,
        used: 0,
    };
}

/// A socket slot and its receive buffer, which bounds the datagrams it queues (8 bytes of overhead each).
pub struct Socket<'a> {
    proto: Option<Proto>,
    port: u16,
    buf: &'a mut [u8],
    head: usize,
    len: usize,
}

impl<'a> Socket<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Socket {
            proto: None,
            port: 0,
            buf,
            head: 0,
            len: 0,
        }
    }

    /// Queues a datagram; with `base` (the checksum's sum over the headers), only if the checksum holds.
    fn push(&mut self, from: SocketAddrV4, data: &[u8], base: Option<u64>) -> Result<(), Reason> {
        let need = RECORD + data.len();
        if need > self.buf.len() - self.len {
            return Err(Reason::SocketFull);
        }
        let mut record = [0; RECORD];
        record[..2].copy_from_slice(&(data.len() as u16).to_be_bytes());
        record[2..6].copy_from_slice(&from.ip().octets());
        record[6..].copy_from_slice(&from.port().to_be_bytes());
        let tail = (self.head + self.len) % self.buf.len();
        ring_write(self.buf, tail, &record);
        let sum = ring_write(self.buf, (tail + RECORD) % self.buf.len(), data);
        if base.is_some_and(|base| fold(base + sum) != 0xffff) {
            return Err(Reason::Checksum);
        }
        self.len += need;
        Ok(())
    }
}

/// A bound socket: an index into the table given to `Stack::new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketId(usize);

pub struct Stack<'a> {
    config: Config,
    neighbors: &'a mut [Neighbor],
    sockets: &'a mut [Socket<'a>],
    ip_id: u16,
    /// A reply built while the received frame is borrowed, sent once it is released.
    reply: [u8; MAX_FRAME],
    reply_len: usize,
    tcp: Tcp<'a>,
    pub counters: Counters,
}

impl<'a> Stack<'a> {
    pub fn new(
        config: Config,
        neighbors: &'a mut [Neighbor],
        sockets: &'a mut [Socket<'a>],
    ) -> Self {
        Stack {
            config,
            neighbors,
            sockets,
            ip_id: 0,
            reply: [0; MAX_FRAME],
            reply_len: 0,
            tcp: Tcp::new([0; 2], &mut [], &mut [], &mut []),
            counters: Counters::default(),
        }
    }

    /// Binds a UDP port or an ICMP echo identifier.
    pub fn bind(&mut self, proto: Proto, port: u16) -> Result<SocketId, Error> {
        if port == 0 {
            return Err(Error::Invalid);
        }
        if self
            .sockets
            .iter()
            .any(|s| s.proto == Some(proto) && s.port == port)
        {
            return Err(Error::InUse);
        }
        let i = self
            .sockets
            .iter()
            .position(|s| s.proto.is_none())
            .ok_or(Error::TableFull)?;
        let s = &mut self.sockets[i];
        (s.proto, s.port, s.head, s.len) = (Some(proto), port, 0, 0);
        Ok(SocketId(i))
    }

    /// Frees the slot; queued datagrams are discarded.
    pub fn close(&mut self, id: SocketId) {
        if let Some(s) = self.sockets.get_mut(id.0) {
            s.proto = None;
        }
    }

    /// Sends one datagram: UDP data, or a whole ICMP echo request (identifier and checksum are set here).
    pub fn send_to(
        &mut self,
        nic: &mut impl Nic,
        now: u64,
        id: SocketId,
        to: SocketAddrV4,
        data: &[u8],
    ) -> Result<(), Error> {
        let s = self.sockets.get(id.0).ok_or(Error::Closed)?;
        let (proto, port) = (s.proto.ok_or(Error::Closed)?, s.port);
        if proto == Proto::Icmp && !(data.len() >= ICMP && data[0] == ECHO_REQUEST && data[1] == 0)
        {
            return Err(Error::Invalid);
        }
        let len = IP + if proto == Proto::Udp { UDP } else { 0 } + data.len();
        if len > nic.mtu().min(MAX_PACKET) {
            return Err(Error::TooBig);
        }
        let dst = *to.ip();
        let next = self.next_hop(dst)?;
        let mac = self.resolve(nic, next, now)?;
        let (src, ours, id) = (self.config.ip, nic.mac(), self.next_id());
        let sent = nic.transmit(ETH + len, |f| {
            write_eth(f, mac, ours, TYPE_IPV4);
            let num = if proto == Proto::Udp {
                PROTO_UDP
            } else {
                PROTO_ICMP
            };
            write_ip(&mut f[ETH..], src, dst, num, len, id);
            let seg = &mut f[ETH + IP..ETH + len];
            match proto {
                Proto::Udp => {
                    let n = (UDP + data.len()) as u16;
                    seg[..2].copy_from_slice(&port.to_be_bytes());
                    seg[2..4].copy_from_slice(&to.port().to_be_bytes());
                    seg[4..6].copy_from_slice(&n.to_be_bytes());
                    seg[6..8].fill(0);
                    let s = pseudo(src, dst, PROTO_UDP, n)
                        + sum(&seg[..UDP])
                        + copy_sum(&mut seg[UDP..], data);
                    // 0 means "no checksum" in UDP, so a computed 0 is sent as its other form.
                    let c = match !fold(s) as u16 {
                        0 => 0xffff,
                        c => c,
                    };
                    seg[6..8].copy_from_slice(&c.to_be_bytes());
                }
                Proto::Icmp => {
                    seg[..4].copy_from_slice(&[ECHO_REQUEST, 0, 0, 0]);
                    seg[4..6].copy_from_slice(&port.to_be_bytes());
                    seg[6..8].copy_from_slice(&data[6..8]);
                    let s = sum(&seg[..ICMP]) + copy_sum(&mut seg[ICMP..], &data[ICMP..]);
                    seg[2..4].copy_from_slice(&(!fold(s) as u16).to_be_bytes());
                }
            }
        });
        self.count_tx(sent);
        if sent { Ok(()) } else { Err(Error::Busy) }
    }

    /// Takes the next queued datagram, copying as much as fits into `buf`; returns its source and the bytes copied.
    pub fn recv_from(&mut self, id: SocketId, buf: &mut [u8]) -> Option<(SocketAddrV4, usize)> {
        let s = self
            .sockets
            .get_mut(id.0)
            .filter(|s| s.proto.is_some() && s.len > 0)?;
        let mut record = [0; RECORD];
        ring_read(s.buf, s.head, &mut record);
        let len = u16::from_be_bytes([record[0], record[1]]) as usize;
        let ip = Ipv4Addr::new(record[2], record[3], record[4], record[5]);
        let n = len.min(buf.len());
        ring_read(s.buf, (s.head + RECORD) % s.buf.len(), &mut buf[..n]);
        s.head = (s.head + RECORD + len) % s.buf.len();
        s.len -= RECORD + len;
        if s.len == 0 {
            s.head = 0;
        }
        Some((
            SocketAddrV4::new(ip, u16::from_be_bytes([record[6], record[7]])),
            n,
        ))
    }

    /// Handles every received frame and due timer; returns the next deadline, if any.
    pub fn poll(&mut self, nic: &mut impl Nic, now: u64) -> Option<u64> {
        let (ours, mss) = (nic.mac(), tcp::mss(nic));
        while nic.receive(|frame| {
            self.counters.rx += 1;
            if let Err(d) = self.handle(frame, ours, now, mss) {
                let c = &mut self.counters;
                *match d {
                    Reason::Malformed => &mut c.malformed,
                    Reason::Checksum => &mut c.checksum,
                    Reason::Fragment => &mut c.fragments,
                    Reason::Ignored => &mut c.ignored,
                    Reason::NoSocket => &mut c.no_socket,
                    Reason::SocketFull => &mut c.socket_full,
                    Reason::Unacceptable => &mut c.unacceptable,
                } += 1;
            }
        }) {
            if self.reply_len > 0 {
                let reply = &self.reply[..self.reply_len];
                let sent = nic.transmit(reply.len(), |f| f.copy_from_slice(reply));
                self.reply_len = 0;
                self.count_tx(sent);
            }
        }
        let mut next = None::<u64>;
        for i in 0..self.neighbors.len() {
            let n = self.neighbors[i];
            if n.ip.is_unspecified() || n.tries == 0 {
                continue;
            }
            let mut due = n.at + ARP_RETRY;
            if now >= due {
                if n.tries >= ARP_TRIES {
                    self.neighbors[i] = Neighbor::EMPTY;
                    continue;
                }
                self.request(nic, n.ip);
                let n = &mut self.neighbors[i];
                (n.at, n.tries) = (now, n.tries + 1);
                due = now + ARP_RETRY;
            }
            next = Some(next.map_or(due, |t| t.min(due)));
        }
        earliest(next, self.tcp_poll(nic, now))
    }

    fn handle(&mut self, frame: &[u8], ours: Mac, now: u64, mss: u16) -> Result<(), Reason> {
        let (eth, body) = frame.split_at_checked(ETH).ok_or(Reason::Malformed)?;
        if eth[..6] != ours && eth[..6] != BROADCAST {
            return Err(Reason::Ignored);
        }
        match [eth[12], eth[13]] {
            TYPE_ARP => self.arp_in(eth, body, ours, now),
            TYPE_IPV4 => self.ipv4_in(eth, body, ours, now, mss),
            _ => Err(Reason::Ignored),
        }
    }

    fn arp_in(&mut self, eth: &[u8], body: &[u8], ours: Mac, now: u64) -> Result<(), Reason> {
        let a = body.get(..ARP).ok_or(Reason::Malformed)?;
        if a[..7] != [0, 1, 8, 0, 6, 4, 0] || a[8..14] != eth[6..12] {
            return Err(Reason::Malformed);
        }
        let sha: Mac = a[8..14].try_into().unwrap();
        let spa = ip_at(&a[14..18]);
        if ip_at(&a[24..28]) != self.config.ip {
            return Err(Reason::Ignored);
        }
        if !self.on_link(spa) || !unicast_mac(sha) {
            return Err(Reason::Malformed);
        }
        match a[7] {
            1 => {
                write_arp(&mut self.reply, 2, ours, self.config.ip, sha, spa);
                self.reply_len = ETH + ARP;
                Ok(())
            }
            2 => {
                let i = self
                    .find(spa)
                    .filter(|&i| self.neighbors[i].tries > 0)
                    .ok_or(Reason::Ignored)?;
                self.learn(i, spa, sha, now);
                Ok(())
            }
            _ => Err(Reason::Malformed),
        }
    }

    fn ipv4_in(
        &mut self,
        eth: &[u8],
        body: &[u8],
        ours: Mac,
        now: u64,
        mss: u16,
    ) -> Result<(), Reason> {
        let h = body.get(..IP).ok_or(Reason::Malformed)?;
        let ihl = (h[0] & 15) as usize * 4;
        let total = u16::from_be_bytes([h[2], h[3]]) as usize;
        if h[0] >> 4 != 4 || ihl < IP || total < ihl || total > body.len() {
            return Err(Reason::Malformed);
        }
        let (h, payload) = body[..total].split_at(ihl);
        if fold(sum(h)) != 0xffff {
            return Err(Reason::Checksum);
        }
        // More fragments, or a fragment offset.
        if h[6] & 0x3f != 0 || h[7] != 0 {
            return Err(Reason::Fragment);
        }
        let (src, dst) = (ip_at(&h[12..16]), ip_at(&h[16..20]));
        if dst != self.config.ip {
            return Err(Reason::Ignored);
        }
        if !unicast(src) {
            return Err(Reason::Malformed);
        }
        match h[9] {
            PROTO_ICMP => self.icmp_in(eth, src, payload, ours),
            PROTO_UDP => self.udp_in(src, payload),
            tcp::PROTO_TCP => self.tcp_in(eth, src, payload, ours, now, mss),
            _ => Err(Reason::Ignored),
        }
    }

    fn icmp_in(&mut self, eth: &[u8], src: Ipv4Addr, msg: &[u8], ours: Mac) -> Result<(), Reason> {
        if msg.len() < ICMP {
            return Err(Reason::Malformed);
        }
        match (msg[0], msg[1]) {
            (ECHO_REQUEST, 0) => {
                let to: Mac = eth[6..12].try_into().unwrap();
                let len = IP + msg.len();
                if len > MAX_PACKET || !unicast_mac(to) {
                    return Err(Reason::Malformed);
                }
                let r = &mut self.reply[ETH + IP..ETH + len];
                if fold(copy_sum(r, msg)) != 0xffff {
                    return Err(Reason::Checksum);
                }
                r[..4].copy_from_slice(&[ECHO_REPLY, 0, 0, 0]);
                let c = !fold(sum(r)) as u16;
                r[2..4].copy_from_slice(&c.to_be_bytes());
                let id = self.next_id();
                write_eth(&mut self.reply, to, ours, TYPE_IPV4);
                write_ip(
                    &mut self.reply[ETH..],
                    self.config.ip,
                    src,
                    PROTO_ICMP,
                    len,
                    id,
                );
                self.reply_len = ETH + len;
                Ok(())
            }
            (ECHO_REPLY, 0) => {
                let id = u16::from_be_bytes([msg[4], msg[5]]);
                self.deliver(Proto::Icmp, id, SocketAddrV4::new(src, 0), msg, Some(0))
            }
            (DEST_UNREACHABLE | TIME_EXCEEDED, code) => {
                if fold(sum(msg)) != 0xffff {
                    return Err(Reason::Checksum);
                }
                let hard = msg[0] == DEST_UNREACHABLE && matches!(code, 2..=4);
                self.tcp_icmp(&msg[ICMP..], hard)
            }
            _ => Err(Reason::Ignored),
        }
    }

    fn udp_in(&mut self, src: Ipv4Addr, seg: &[u8]) -> Result<(), Reason> {
        let h = seg.get(..UDP).ok_or(Reason::Malformed)?;
        let len = u16::from_be_bytes([h[4], h[5]]);
        if (len as usize) < UDP || len as usize > seg.len() {
            return Err(Reason::Malformed);
        }
        let base =
            (h[6..8] != [0, 0]).then(|| pseudo(src, self.config.ip, PROTO_UDP, len) + sum(h));
        let from = SocketAddrV4::new(src, u16::from_be_bytes([h[0], h[1]]));
        self.deliver(
            Proto::Udp,
            u16::from_be_bytes([h[2], h[3]]),
            from,
            &seg[UDP..len as usize],
            base,
        )
    }

    fn deliver(
        &mut self,
        proto: Proto,
        port: u16,
        from: SocketAddrV4,
        data: &[u8],
        base: Option<u64>,
    ) -> Result<(), Reason> {
        let s = self
            .sockets
            .iter_mut()
            .find(|s| s.proto == Some(proto) && s.port == port)
            .ok_or(Reason::NoSocket)?;
        s.push(from, data, base)
    }

    /// The MAC to send to `ip` through, sending an ARP request when it is unknown or stale.
    fn resolve(&mut self, nic: &mut impl Nic, ip: Ipv4Addr, now: u64) -> Result<Mac, Error> {
        let Some(i) = self.find(ip) else {
            let i = self.slot().ok_or(Error::Unresolved)?;
            self.neighbors[i] = Neighbor {
                ip,
                at: now,
                tries: 1,
                used: now,
                ..Neighbor::EMPTY
            };
            self.request(nic, ip);
            return Err(Error::Unresolved);
        };
        let n = &mut self.neighbors[i];
        n.used = now;
        let mac = n.mac.ok_or(Error::Unresolved)?;
        if n.tries == 0 && now.saturating_sub(n.at) >= ARP_STALE {
            (n.at, n.tries) = (now, 1);
            self.request(nic, ip);
        }
        Ok(mac)
    }

    fn request(&mut self, nic: &mut impl Nic, ip: Ipv4Addr) {
        let (ours, me) = (nic.mac(), self.config.ip);
        let sent = nic.transmit(ETH + ARP, |f| write_arp(f, 1, ours, me, [0; 6], ip));
        self.count_tx(sent);
    }

    fn learn(&mut self, i: usize, ip: Ipv4Addr, mac: Mac, now: u64) {
        self.neighbors[i] = Neighbor {
            ip,
            mac: Some(mac),
            at: now,
            used: now,
            ..Neighbor::EMPTY
        };
    }

    fn find(&self, ip: Ipv4Addr) -> Option<usize> {
        self.neighbors.iter().position(|n| n.ip == ip)
    }

    /// A free entry, else the least recently used.
    fn slot(&self) -> Option<usize> {
        (0..self.neighbors.len()).min_by_key(|&i| {
            (
                !self.neighbors[i].ip.is_unspecified(),
                self.neighbors[i].used,
            )
        })
    }

    /// A unicast address on our subnet, other than ours.
    fn on_link(&self, ip: Ipv4Addr) -> bool {
        let (ip, me, mask) = (
            u32::from(ip),
            u32::from(self.config.ip),
            u32::from(self.config.netmask),
        );
        ip & mask == me & mask && ip != me && ip & !mask != 0 && ip & !mask != !mask
    }

    fn next_hop(&self, dst: Ipv4Addr) -> Result<Ipv4Addr, Error> {
        let mask = u32::from(self.config.netmask);
        if self.on_link(dst) {
            Ok(dst)
        } else if unicast(dst) && u32::from(dst) & mask != u32::from(self.config.ip) & mask {
            self.config.gateway.ok_or(Error::NoRoute)
        } else {
            Err(Error::NoRoute)
        }
    }

    fn next_id(&mut self) -> u16 {
        self.ip_id = self.ip_id.wrapping_add(1);
        self.ip_id
    }

    fn count_tx(&mut self, sent: bool) {
        if sent {
            self.counters.tx += 1;
        } else {
            self.counters.tx_busy += 1;
        }
    }
}

fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

fn ip_at(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

fn unicast(ip: Ipv4Addr) -> bool {
    !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast())
}

fn unicast_mac(mac: Mac) -> bool {
    mac[0] & 1 == 0 && mac != [0; 6]
}

fn write_arp(f: &mut [u8], op: u8, sha: Mac, spa: Ipv4Addr, tha: Mac, tpa: Ipv4Addr) {
    write_eth(f, if op == 1 { BROADCAST } else { tha }, sha, TYPE_ARP);
    let a = &mut f[ETH..ETH + ARP];
    a[..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, op]);
    a[8..14].copy_from_slice(&sha);
    a[14..18].copy_from_slice(&spa.octets());
    a[18..24].copy_from_slice(&tha);
    a[24..].copy_from_slice(&tpa.octets());
}

/// An IPv4 header without options, Don't Fragment set; `len` is the whole packet's.
fn write_ip(f: &mut [u8], src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize, id: u16) {
    let h = &mut f[..IP];
    let [l0, l1] = (len as u16).to_be_bytes();
    let [i0, i1] = id.to_be_bytes();
    h[..12].copy_from_slice(&[0x45, 0, l0, l1, i0, i1, 0x40, 0, 64, proto, 0, 0]);
    h[12..16].copy_from_slice(&src.octets());
    h[16..].copy_from_slice(&dst.octets());
    let c = !fold(sum(h)) as u16;
    h[10..12].copy_from_slice(&c.to_be_bytes());
}

fn pseudo(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: u16) -> u64 {
    let (s, d) = (u32::from(src) as u64, u32::from(dst) as u64);
    (s >> 16) + (s & 0xffff) + (d >> 16) + (d & 0xffff) + proto as u64 + len as u64
}

fn write_eth(f: &mut [u8], dst: Mac, src: Mac, kind: [u8; 2]) {
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&src);
    f[12..ETH].copy_from_slice(&kind);
}

/// Writes `src` into the ring `buf` from `pos`, wrapping; returns its checksum sum.
fn ring_write(buf: &mut [u8], pos: usize, src: &[u8]) -> u64 {
    let first = (buf.len() - pos).min(src.len());
    let (a, b) = src.split_at(first);
    let sa = copy_sum(&mut buf[pos..pos + first], a);
    let sb = copy_sum(&mut buf[..b.len()], b);
    // A piece that starts at an odd offset sums with its bytes swapped.
    sa + if first % 2 == 1 { swap(sb) } else { sb }
}

fn ring_put(buf: &mut [u8], pos: usize, src: &[u8]) {
    let first = (buf.len() - pos).min(src.len());
    let (a, b) = src.split_at(first);
    buf[pos..pos + first].copy_from_slice(a);
    buf[..b.len()].copy_from_slice(b);
}

fn ring_read(buf: &[u8], pos: usize, dst: &mut [u8]) {
    let first = (buf.len() - pos).min(dst.len());
    let (a, b) = dst.split_at_mut(first);
    a.copy_from_slice(&buf[pos..pos + first]);
    let n = b.len();
    b.copy_from_slice(&buf[..n]);
}

fn swap(s: u64) -> u64 {
    let s = fold(s);
    ((s & 0xff) << 8) | (s >> 8)
}

/// Folds a ones' complement sum to 16 bits.
fn fold(mut s: u64) -> u64 {
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    s
}

/// The ones' complement sum of `data` as big-endian 16-bit words (an odd last byte padded with zero), unfolded.
fn sum(data: &[u8]) -> u64 {
    let (words, rest) = data.as_chunks::<4>();
    let mut tail = [0; 4];
    tail[..rest.len()].copy_from_slice(rest);
    words
        .iter()
        .chain([&tail])
        .fold(0, |acc, &w| acc + u32::from_be_bytes(w) as u64)
}

/// Copies `src` into `dst` (the same length) and returns `sum(src)`.
fn copy_sum(dst: &mut [u8], src: &[u8]) -> u64 {
    dst.copy_from_slice(src);
    sum(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_write_sums_like_a_contiguous_copy() {
        let src: Vec<u8> = (0..23u8).map(|i| i.wrapping_mul(37) ^ 0xa5).collect();
        for len in 0..=src.len() {
            for pos in 0..src.len() {
                let mut ring = [0u8; 23];
                let s = ring_write(&mut ring, pos, &src[..len]);
                assert_eq!(fold(s), fold(sum(&src[..len])), "len {len} at {pos}");
                let mut back = vec![0; len];
                ring_read(&ring, pos, &mut back);
                assert_eq!(back, &src[..len]);
            }
        }
    }
}
