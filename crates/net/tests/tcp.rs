mod sim;

use std::net::{Ipv4Addr, SocketAddrV4};

use net::{
    Config, Counters, Error, HalfOpen, Mac, Neighbor, Stack, State, Tcp, TcpId, TcpSocket, TimeWait,
};
use sim::{Faults, Link, Rng, Tap, mutate};

const MAC_A: Mac = [2, 0, 0, 0, 0, 1];
const MAC_B: Mac = [2, 0, 0, 0, 0, 2];
const IP_A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const PORT: u16 = 80;
const PEER_PORT: u16 = 40000;
const FIN: u8 = 1;
const SYN: u8 = 2;
const RST: u8 = 4;
const ACK: u8 = 16;
/// The pattern's period: prime, so it never lines up with a ring.
const PERIOD: usize = 65521;

fn config(ip: Ipv4Addr) -> Config {
    Config {
        ip,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: None,
    }
}

fn drops(c: &Counters) -> u64 {
    c.malformed
        + c.checksum
        + c.fragments
        + c.ignored
        + c.no_socket
        + c.socket_full
        + c.unacceptable
}

/// A stack's memory: `slots` connections with `rx`/`tx`-byte rings, and the half-open and TIME_WAIT tables.
struct Mem {
    neighbors: [Neighbor; 4],
    rx: Vec<Vec<u8>>,
    tx: Vec<Vec<u8>>,
    half_open: Vec<HalfOpen>,
    time_wait: Vec<TimeWait>,
}

impl Mem {
    fn new(slots: usize, rx: usize, tx: usize, half_open: usize, time_wait: usize) -> Self {
        Mem {
            neighbors: [Neighbor::EMPTY; 4],
            rx: vec![vec![0; rx]; slots],
            tx: vec![vec![0; tx]; slots],
            half_open: vec![HalfOpen::EMPTY; half_open],
            time_wait: vec![TimeWait::EMPTY; time_wait],
        }
    }
}

/// Binds `$stack` to a TCP stack at `$ip` over `$mem`.
macro_rules! host {
    ($stack:ident, $mem:expr, $ip:expr, $key:expr) => {
        let mem = &mut $mem;
        let mut socks: Vec<TcpSocket> = mem
            .rx
            .iter_mut()
            .zip(mem.tx.iter_mut())
            .map(|(r, t)| TcpSocket::new(r, t))
            .collect();
        #[allow(unused_mut)]
        let mut $stack = Stack::new(config($ip), &mut mem.neighbors, &mut []).with_tcp(Tcp::new(
            $key,
            &mut socks,
            &mut mem.half_open,
            &mut mem.time_wait,
        ));
    };
}

fn pattern() -> Vec<u8> {
    (0..2 * PERIOD)
        .map(|k| {
            ((k % PERIOD) as u32)
                .wrapping_mul(2_654_435_761)
                .to_be_bytes()[0]
        })
        .collect()
}

/// One end of a transfer: what it sent and received.
#[derive(Default)]
struct Flow {
    sent: usize,
    shut: bool,
    received: usize,
    eof: bool,
}

/// Sends the next part of `bytes` of the pattern, shuts down after the last, and checks everything received.
fn pump(
    stack: &mut Stack,
    id: TcpId,
    f: &mut Flow,
    bytes: usize,
    pat: &[u8],
    buf: &mut [u8],
) -> bool {
    let mut progress = false;
    while f.sent < bytes {
        let at = f.sent % PERIOD;
        let n = (bytes - f.sent).min(PERIOD);
        match stack.send(id, &pat[at..at + n]) {
            Ok(n) => (f.sent, progress) = (f.sent + n, true),
            Err(Error::WouldBlock) => break,
            Err(e) => panic!("send: {e:?}"),
        }
    }
    if f.sent == bytes && !f.shut {
        stack.shutdown(id);
        (f.shut, progress) = (true, true);
    }
    loop {
        match stack.recv(id, buf) {
            Ok(0) => {
                progress |= !f.eof;
                f.eof = true;
                break;
            }
            Ok(n) => {
                let at = f.received % PERIOD;
                assert!(buf[..n] == pat[at..at + n], "data at {} intact", f.received);
                (f.received, progress) = (f.received + n, true);
            }
            Err(Error::WouldBlock) => break,
            Err(e) => panic!("recv: {e:?} {:?} {:?}", stack.tcp_info(id), stack.counters),
        }
    }
    progress
}

/// Steps both stacks over the link until `done`, jumping virtual time to the next event; panics past `limit`.
fn run(
    link: &mut Link,
    a: &mut Stack,
    b: &mut Stack,
    limit: u64,
    mut step: impl FnMut(&mut Stack, &mut Stack, u64) -> (bool, bool),
) {
    let mut idle = 0;
    loop {
        let now = link.now;
        let da = a.poll(&mut link.end(0), now);
        let db = b.poll(&mut link.end(1), now);
        let (progress, done) = step(a, b, now);
        if done {
            return;
        }
        if progress {
            continue;
        }
        let next = [da, db, link.next()]
            .into_iter()
            .flatten()
            .min()
            .expect("stalled with nothing in flight and no timer");
        assert!(next < limit, "not done by {} ms", limit / MS);
        idle = if next <= now { idle + 1 } else { 0 };
        assert!(idle < 1000, "spinning at {now}");
        link.now = next.max(now);
    }
}

/// When A's first connect, at that time, gets an ISN `before` below 2^32 (RFC 6528's clock moves it 1 per 4 us).
fn wrap_start(key: [u64; 2], before: u32) -> u64 {
    let mut m = Mem::new(1, 0, 0, 0, 0);
    host!(a, m, IP_A, key);
    let mut tap = Tap::new(MAC_A);
    a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let isn = feed(&mut a, &mut tap, 0, [])[0].seq;
    0u32.wrapping_sub(before).wrapping_sub(isn) as u64 * 4000
}

/// Liveness, after a poll: a connection with work outstanding (anything but idle in ESTABLISHED or CLOSE-WAIT, or
/// an open FIN-WAIT-2) has a deadline.
fn live(s: &Stack, id: TcpId) {
    let i = s.tcp_info(id).unwrap();
    let idle = matches!(i.state, State::Closed | State::Listen)
        || (matches!(i.state, State::Established | State::CloseWait) && i.queued == 0)
        || (i.state == State::FinWait2 && !i.released);
    assert!(
        idle || i.deadline.is_some(),
        "work outstanding and no deadline: {i:?}"
    );
}

/// A connects to B and both send `bytes` of the pattern at once, then close; every byte and both ends of stream
/// are checked. A's ISN sits half the transfer below 2^32, so A's send and B's receive sequence numbers wrap.
fn transfer(seed: u64, faults: Faults, bytes: usize, ring: usize) -> u64 {
    let (ka, kb) = ([seed, 0xa], [seed, 0xb]);
    let start = wrap_start(ka, (bytes / 2) as u32);
    let mut link = Link::new(seed, faults, [MAC_A, MAC_B]);
    link.now = start;
    let (mut ma, mut mb) = (Mem::new(1, ring, ring, 4, 4), Mem::new(2, ring, ring, 4, 4));
    host!(a, ma, IP_A, ka);
    host!(b, mb, IP_B, kb);
    let listener = b.listen(PORT).unwrap();
    let ca = a.connect(start, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let mut cb = None;
    let (mut fa, mut fb) = (Flow::default(), Flow::default());
    let (pat, mut buf) = (pattern(), vec![0u8; 8192]);
    run(&mut link, &mut a, &mut b, start + 3600 * SEC, |a, b, _| {
        // Checked right after the polls: a deadline moves when the next poll acts on the step's calls.
        live(a, ca);
        cb = cb.or_else(|| b.accept(listener));
        let mut progress = pump(a, ca, &mut fa, bytes, &pat, &mut buf);
        if let Some(cb) = cb {
            live(b, cb);
            progress |= pump(b, cb, &mut fb, bytes, &pat, &mut buf);
        }
        let closed = |s: &Stack, id| s.tcp_info(id).unwrap().state == State::Closed;
        let done = fa.eof && fb.eof && closed(a, ca) && cb.is_some_and(|cb| closed(b, cb));
        (progress, done)
    });
    assert_eq!((fa.received, fb.received), (bytes, bytes));
    assert_eq!(a.tcp_info(ca).unwrap().error, None);
    link.now - start
}

fn faults(loss: u64) -> Faults {
    Faults {
        loss,
        duplicate: 10,
        reorder: 20,
        corrupt: 5,
        delay: MS,
        arp_loss: 300,
    }
}

#[test]
fn one_mib_each_way_arrives_intact_without_loss() {
    for seed in 0..200 {
        transfer(seed, faults(0), 1 << 20, 64 << 10);
    }
}

#[test]
fn one_mib_each_way_arrives_intact_at_1_percent_loss() {
    for seed in 0..200 {
        transfer(seed, faults(10), 1 << 20, 64 << 10);
    }
}

#[test]
fn one_mib_each_way_arrives_intact_at_5_percent_loss() {
    for seed in 0..200 {
        transfer(seed, faults(50), 1 << 20, 64 << 10);
    }
}

#[test]
fn sixty_four_mib_each_way_arrives_intact_at_0_1_and_5_percent_loss() {
    for loss in [0, 10, 50] {
        for seed in 0..3 {
            transfer(seed, faults(loss), 64 << 20, 256 << 10);
        }
    }
}

#[test]
#[ignore = "soak: 64 MiB each way for 200 seeds at each loss rate, about 43 s"]
fn sixty_four_mib_soak() {
    for loss in [0, 10, 50] {
        for seed in 0..200 {
            transfer(seed, faults(loss), 64 << 20, 256 << 10);
        }
    }
}

// Segments crafted by a scripted peer at IP_B against our stack at IP_A on a tap.

struct Seg {
    seq: u32,
    ack: u32,
    flags: u8,
    win: u16,
    data: Vec<u8>,
}

fn parse(f: &[u8]) -> Option<Seg> {
    if f.get(12..14)? != [8, 0] || *f.get(23)? != 6 {
        return None;
    }
    let t = &f[34..];
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some(Seg {
        seq: be32(&t[4..]),
        ack: be32(&t[8..]),
        flags: t[13],
        win: u16::from_be_bytes([t[14], t[15]]),
        data: t[(t[12] >> 4) as usize * 4..].to_vec(),
    })
}

fn csum(chunks: &[&[u8]]) -> u16 {
    let mut s = 0u32;
    for c in chunks {
        for w in c.chunks(2) {
            s += u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]) as u32;
        }
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

fn ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut h = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, proto, 0, 0];
    h[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    h.extend_from_slice(&src.octets());
    h.extend_from_slice(&dst.octets());
    let c = csum(&[&h]);
    h[10..12].copy_from_slice(&c.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

fn ether(dst: Mac, src: Mac, ip: Vec<u8>) -> Vec<u8> {
    let mut f = dst.to_vec();
    f.extend_from_slice(&src);
    f.extend_from_slice(&[8, 0]);
    f.extend(ip);
    f
}

/// The ARP reply from `from` to `to`'s request.
fn arp_reply(from: (Mac, Ipv4Addr), to: (Mac, Ipv4Addr)) -> Vec<u8> {
    let mut f = to.0.to_vec();
    f.extend_from_slice(&from.0);
    f.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 2]);
    f.extend_from_slice(&from.0);
    f.extend_from_slice(&from.1.octets());
    f.extend_from_slice(&to.0);
    f.extend_from_slice(&to.1.octets());
    f
}

/// A peer at IP_B (MAC_B) that builds segments to our stack at IP_A by hand.
struct Peer {
    port: u16,
    to: u16,
    seq: u32,
    ack: u32,
}

impl Peer {
    fn seg(&self, seq: u32, ack: u32, flags: u8, win: u16, opts: &[u8], data: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&self.port.to_be_bytes());
        t.extend_from_slice(&self.to.to_be_bytes());
        t.extend_from_slice(&seq.to_be_bytes());
        t.extend_from_slice(&ack.to_be_bytes());
        t.extend_from_slice(&[(((20 + opts.len()) / 4) << 4) as u8, flags]);
        t.extend_from_slice(&win.to_be_bytes());
        t.extend_from_slice(&[0; 4]);
        t.extend_from_slice(opts);
        t.extend_from_slice(data);
        let mut pseudo = IP_B.octets().to_vec();
        pseudo.extend_from_slice(&IP_A.octets());
        pseudo.extend_from_slice(&[0, 6]);
        pseudo.extend_from_slice(&(t.len() as u16).to_be_bytes());
        let c = csum(&[&pseudo, &t]);
        t[16..18].copy_from_slice(&c.to_be_bytes());
        ether(MAC_A, MAC_B, ipv4(IP_B, IP_A, 6, &t))
    }

    /// A segment at the peer's current sequence and acknowledgment numbers.
    fn now(&self, flags: u8, data: &[u8]) -> Vec<u8> {
        self.seg(self.seq, self.ack, flags, 65535, &[], data)
    }
}

/// Feeds frames to `stack` and returns the TCP segments it sent in answer; the peer answers its ARP requests.
fn feed(
    stack: &mut Stack,
    tap: &mut Tap,
    now: u64,
    frames: impl IntoIterator<Item = Vec<u8>>,
) -> Vec<Seg> {
    tap.rx.extend(frames);
    tap.tx.clear();
    stack.poll(tap, now);
    if tap.tx.iter().any(|f| f[12..14] == [8, 6]) {
        tap.rx.push_back(arp_reply((MAC_B, IP_B), (MAC_A, IP_A)));
        stack.poll(tap, now);
    }
    tap.tx.iter().filter_map(|f| parse(f)).collect()
}

/// A listener on PORT accepts a connection from the peer (no window scaling, MSS 1460); returns it and the peer.
fn accepted(a: &mut Stack, tap: &mut Tap) -> (TcpId, Peer) {
    let listener = a.listen(PORT).unwrap();
    let mut p = Peer {
        port: PEER_PORT,
        to: PORT,
        seq: 1000,
        ack: 0,
    };
    let synack = feed(
        a,
        tap,
        0,
        [p.seg(1000, 0, SYN, 65535, &[2, 4, 5, 0xb4], &[])],
    );
    assert_eq!(synack.len(), 1);
    assert_eq!((synack[0].flags, synack[0].ack), (SYN | ACK, 1001));
    (p.seq, p.ack) = (1001, synack[0].seq.wrapping_add(1));
    assert!(feed(a, tap, 0, [p.now(ACK, &[])]).is_empty());
    (a.accept(listener).unwrap(), p)
}

#[test]
fn rfc5961_rst_and_syn_get_rate_limited_challenge_acks_per_connection() {
    let mut m = Mem::new(3, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 2]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);

    let out = feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq + 100, p.ack, RST, 0, &[], &[])],
    );
    assert_eq!(
        out.len(),
        1,
        "an inexact in-window RST gets a challenge ACK"
    );
    assert_eq!((out[0].flags, out[0].seq, out[0].ack), (ACK, p.ack, p.seq));
    assert!(
        feed(
            &mut a,
            &mut tap,
            0,
            [p.seg(p.seq + 1_000_000, p.ack, RST, 0, &[], &[])]
        )
        .is_empty()
    );
    let out = feed(&mut a, &mut tap, 0, [p.seg(p.seq + 7, 0, SYN, 0, &[], &[])]);
    assert_eq!(
        (out.len(), out[0].flags),
        (1, ACK),
        "a SYN gets a challenge ACK"
    );
    let out = feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq + 1_000_000, 0, SYN, 0, &[], &[])],
    );
    assert_eq!(out.len(), 1, "even one out of the window");
    assert_eq!(a.counters.challenge_acks, 3);
    assert_eq!(a.tcp_info(c).unwrap().state, State::Established);

    let flood = (0..100).map(|i| p.seg(p.seq + 1 + i, p.ack, RST, 0, &[], &[]));
    assert_eq!(
        feed(&mut a, &mut tap, 0, flood).len(),
        7,
        "10 per second per connection"
    );
    assert_eq!(a.counters.challenge_acks, 10);
    assert_eq!(a.tcp_info(c).unwrap().state, State::Established);

    // A second connection has its own budget (CVE-2016-5696 came from one shared limit).
    let mut q = Peer {
        port: PEER_PORT + 1,
        ..p
    };
    let synack = feed(&mut a, &mut tap, 0, [q.seg(5000, 0, SYN, 65535, &[], &[])]);
    (q.seq, q.ack) = (5001, synack[0].seq + 1);
    feed(&mut a, &mut tap, 0, [q.now(ACK, &[])]);
    let flood = (0..20).map(|i| q.seg(q.seq + 1 + i, q.ack, RST, 0, &[], &[]));
    assert_eq!(feed(&mut a, &mut tap, 0, flood).len(), 10);
    let out = feed(
        &mut a,
        &mut tap,
        SEC,
        [p.seg(p.seq + 3, p.ack, RST, 0, &[], &[])],
    );
    assert_eq!(out.len(), 1, "a new second, a new budget");

    assert!(
        feed(
            &mut a,
            &mut tap,
            SEC,
            [p.seg(p.seq, p.ack, RST, 0, &[], &[])]
        )
        .is_empty()
    );
    assert_eq!(
        a.recv(c, &mut [0; 8]),
        Err(Error::Reset),
        "an exact RST resets"
    );
}

#[test]
fn acks_above_snd_nxt_are_ignored_and_ack_division_does_not_grow_cwnd() {
    let mut m = Mem::new(2, 4096, 64 << 10, 4, 4);
    host!(a, m, IP_A, [1, 2]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    a.send(c, &[7; 30_000]).unwrap();
    let sent = feed(&mut a, &mut tap, 0, []);
    let cwnd = a.tcp_info(c).unwrap().cwnd;
    assert_eq!(cwnd, 3 * 1460, "RFC 5681 initial window");
    assert_eq!(
        sent.iter().map(|s| s.data.len()).sum::<usize>(),
        cwnd as usize
    );
    let snd_max = p.ack + cwnd;

    let updates = [40000, 40001, 40002].map(|w| p.seg(p.seq, p.ack, ACK, w, &[], &[]));
    assert!(
        feed(&mut a, &mut tap, 0, updates).is_empty(),
        "window updates are not duplicate ACKs (RFC 5681)"
    );
    assert_eq!(a.tcp_info(c).unwrap().cwnd, cwnd);

    let before = a.counters;
    let out = feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, snd_max + 1000, ACK, 65535, &[], b"x")],
    );
    assert_eq!(
        (out.len(), out[0].ack),
        (1, p.seq),
        "challenge ACK, data dropped"
    );
    assert_eq!(a.counters.unacceptable, before.unacceptable + 1);
    assert_eq!(a.recv(c, &mut [0; 8]), Err(Error::WouldBlock));
    assert_eq!(a.tcp_info(c).unwrap().cwnd, cwnd);

    let split = (1..=100).map(|i| p.seg(p.seq, p.ack + i, ACK, 65535, &[], &[]));
    feed(&mut a, &mut tap, 0, split);
    assert_eq!(
        a.tcp_info(c).unwrap().cwnd,
        cwnd + 100,
        "100 one-byte ACKs grow cwnd by 100 bytes"
    );
    feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, p.ack + 100 + 1460, ACK, 65535, &[], &[])],
    );
    assert_eq!(a.tcp_info(c).unwrap().cwnd, cwnd + 100 + 1460);
}

fn icmp_unreachable(code: u8, sport: u16, dport: u16, seq: u32) -> Vec<u8> {
    let mut tcp = sport.to_be_bytes().to_vec();
    tcp.extend_from_slice(&dport.to_be_bytes());
    tcp.extend_from_slice(&seq.to_be_bytes());
    let quoted = ipv4(IP_A, IP_B, 6, &tcp);
    let mut m = vec![3, code, 0, 0, 0, 0, 0, 0];
    m.extend_from_slice(&quoted[..28]);
    let c = csum(&[&m]);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    ether(MAC_A, MAC_B, ipv4(IP_B, IP_A, 1, &m))
}

#[test]
fn rfc5927_icmp_errors_must_quote_a_sequence_in_flight_and_never_abort_an_established_connection() {
    let mut m = Mem::new(3, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 2]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    a.send(c, &[1; 100]).unwrap();
    feed(&mut a, &mut tap, 0, []);
    let before = a.counters;
    for seq in [p.ack - 1, p.ack + 100, p.ack + 5000] {
        feed(
            &mut a,
            &mut tap,
            0,
            [icmp_unreachable(3, PORT, PEER_PORT, seq)],
        );
    }
    assert_eq!(a.counters.unacceptable, before.unacceptable + 3);
    feed(
        &mut a,
        &mut tap,
        0,
        [icmp_unreachable(3, PORT, PEER_PORT, p.ack + 50)],
    );
    assert_eq!(a.counters.tcp, before.tcp + 1);
    assert_eq!(
        a.tcp_info(c).unwrap().state,
        State::Established,
        "a hard error is soft once established"
    );
    assert_eq!(a.tcp_info(c).unwrap().error, None);

    let d = a
        .connect(0, 3333, SocketAddrV4::new(IP_B, PEER_PORT))
        .unwrap();
    let syn = feed(&mut a, &mut tap, 0, []);
    let iss = syn[0].seq;
    feed(
        &mut a,
        &mut tap,
        0,
        [icmp_unreachable(3, 3333, PEER_PORT, iss + 1)],
    );
    assert_eq!(a.tcp_info(d).unwrap().state, State::SynSent);
    feed(
        &mut a,
        &mut tap,
        0,
        [icmp_unreachable(3, 3333, PEER_PORT, iss)],
    );
    assert_eq!(a.recv(d, &mut [0; 8]), Err(Error::Unreachable));
}

#[test]
fn ten_thousand_spoofed_syns_then_a_real_client_connects_within_one_rto() {
    let mut mb = Mem::new(2, 4096, 4096, 8, 4);
    host!(b, mb, IP_B, [5, 6]);
    let listener = b.listen(PORT).unwrap();
    let mut tap = Tap::new(MAC_B);
    let mut rng = Rng::new(9);
    let syns: Vec<_> = (0..10_000)
        .map(|_| {
            let p = Peer {
                port: 1024 + rng.below(60_000) as u16,
                to: PORT,
                seq: rng.next() as u32,
                ack: 0,
            };
            let mut f = p.seg(p.seq, 0, SYN, 65535, &[], &[]);
            // Spoofed: random off-link source addresses and MACs (checksums do not cover the MAC).
            f[6..12].copy_from_slice(&[2, 0, 0, rng.next() as u8, rng.next() as u8, 9]);
            let src = Ipv4Addr::from(0x0b00_0000 | rng.below(1 << 24) as u32);
            let mut ip = ipv4(src, IP_B, 6, &f[34..]);
            let mut t = ip.split_off(20);
            let mut pseudo = src.octets().to_vec();
            pseudo.extend_from_slice(&IP_B.octets());
            pseudo.extend_from_slice(&[0, 6, 0, t.len() as u8]);
            t[16..18].fill(0);
            let c = csum(&[&pseudo, &t]);
            t[16..18].copy_from_slice(&c.to_be_bytes());
            ip.extend(t);
            let mut frame = f[..14].to_vec();
            frame[..6].copy_from_slice(&MAC_B);
            frame.extend(ip);
            frame
        })
        .collect();
    let answers = feed(&mut b, &mut tap, 0, syns);
    assert_eq!(answers.len(), 10_000, "every SYN answered by a SYN-ACK");
    assert!(tap.tx.iter().all(|f| f[12..14] == [8, 0]), "no ARP traffic");
    assert_eq!(
        b.counters.syn_cookies,
        10_000 - 8,
        "the table holds 8, the rest get cookies"
    );

    let mut link = Link::new(1, faults(0), [MAC_A, MAC_B]);
    let mut ma = Mem::new(1, 4096, 4096, 4, 4);
    host!(a, ma, IP_A, [7, 8]);
    let start = 10 * MS;
    link.now = start;
    let c = a.connect(start, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let mut served = None;
    run(&mut link, &mut a, &mut b, start + SEC, |a, b, _| {
        served = served.or_else(|| b.accept(listener));
        let up = a.tcp_info(c).unwrap().state == State::Established;
        (false, up && served.is_some())
    });
}

#[test]
fn a_reader_stalled_for_ten_rtos_resumes_through_the_persist_timer() {
    for (seed, loss) in [(1, 0), (2, 0), (3, 50), (4, 50), (5, 50)] {
        let mut link = Link::new(seed, faults(loss), [MAC_A, MAC_B]);
        let (mut ma, mut mb) = (
            Mem::new(1, 4096, 64 << 10, 4, 4),
            Mem::new(2, 16 << 10, 4096, 4, 4),
        );
        host!(a, ma, IP_A, [1, seed]);
        host!(b, mb, IP_B, [2, seed]);
        let listener = b.listen(PORT).unwrap();
        let ca = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
        let (bytes, pat, mut buf) = (256 << 10, pattern(), vec![0u8; 4096]);
        let (mut fa, mut fb, mut cb) = (Flow::default(), Flow::default(), None);
        // The stall: when it ends, and A's frame count when it began.
        let mut stall = None::<(u64, u64)>;
        let mut probes = 0;
        run(&mut link, &mut a, &mut b, 3600 * SEC, |a, b, now| {
            cb = cb.or_else(|| b.accept(listener));
            let mut progress = pump(a, ca, &mut fa, bytes, &pat, &mut buf);
            let Some(cb) = cb else {
                return (progress, false);
            };
            if fb.received >= 64 << 10 && stall.is_none() {
                stall = Some((now + 10 * a.tcp_info(ca).unwrap().rto, a.counters.tx));
            }
            match stall {
                Some((until, _)) if now < until => {}
                Some((_, tx)) if tx > 0 => {
                    probes = a.counters.tx - tx;
                    stall = Some((0, 0));
                }
                _ => progress |= pump(b, cb, &mut fb, 0, &pat, &mut buf),
            }
            (progress, fb.received == bytes && fb.eof)
        });
        assert_eq!(a.tcp_info(ca).unwrap().error, None);
        assert!(
            (3..30).contains(&probes),
            "seed {seed}: {probes} probes while stalled"
        );
    }
}

fn state(s: &Stack, id: TcpId) -> State {
    s.tcp_info(id).unwrap().state
}

#[test]
fn half_close_then_the_other_side_finishes_and_closes() {
    let mut link = Link::new(1, faults(10), [MAC_A, MAC_B]);
    let (mut ma, mut mb) = (Mem::new(1, 8192, 8192, 4, 4), Mem::new(2, 8192, 8192, 4, 4));
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let listener = b.listen(PORT).unwrap();
    let ca = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let (pat, mut buf) = (pattern(), vec![0u8; 4096]);
    let (mut fa, mut fb, mut cb) = (Flow::default(), Flow::default(), None);
    run(&mut link, &mut a, &mut b, 600 * SEC, |a, b, _| {
        cb = cb.or_else(|| b.accept(listener));
        let mut progress = pump(a, ca, &mut fa, 100, &pat, &mut buf);
        let Some(cb) = cb else {
            return (progress, false);
        };
        // B answers only after A's half-close, while A still reads.
        if !fb.eof {
            fb.shut = true;
        } else if fb.sent == 0 && fb.shut {
            assert!(matches!(state(a, ca), State::FinWait1 | State::FinWait2));
            fb.shut = false;
        }
        let answer = if fb.eof { 100 << 10 } else { 0 };
        progress |= pump(b, cb, &mut fb, answer, &pat, &mut buf);
        (progress, fa.eof && state(b, cb) == State::Closed)
    });
    assert_eq!((fb.received, fa.received), (100, 100 << 10));
    // A closed first, so its slot is free and its connection sits in the TIME_WAIT table.
    a.tcp_close(ca);
    assert_eq!(state(&a, ca), State::Closed);
    assert!(
        a.connect(link.now, 0, SocketAddrV4::new(IP_B, PORT))
            .is_ok()
    );
}

#[test]
fn abort_resets_the_peer_and_a_port_without_a_listener_refuses() {
    let mut link = Link::new(1, faults(0), [MAC_A, MAC_B]);
    let (mut ma, mut mb) = (Mem::new(2, 4096, 4096, 4, 4), Mem::new(2, 4096, 4096, 4, 4));
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let listener = b.listen(PORT).unwrap();
    let ca = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let refused = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT + 1)).unwrap();
    let mut cb = None;
    run(&mut link, &mut a, &mut b, 10 * SEC, |a, b, _| {
        cb = cb.or_else(|| b.accept(listener));
        let done = cb.is_some()
            && state(a, ca) == State::Established
            && state(a, refused) == State::Closed;
        (false, done)
    });
    assert_eq!(a.recv(refused, &mut [0; 8]), Err(Error::Refused));
    b.abort(cb.unwrap());
    run(&mut link, &mut a, &mut b, 20 * SEC, |a, _, _| {
        (false, state(a, ca) == State::Closed)
    });
    assert_eq!(a.recv(ca, &mut [0; 8]), Err(Error::Reset));
    assert_eq!(a.send(ca, b"x"), Err(Error::Reset));
}

#[test]
fn simultaneous_open_connects_both_sides() {
    let mut link = Link::new(1, faults(0), [MAC_A, MAC_B]);
    let (mut ma, mut mb) = (Mem::new(1, 4096, 4096, 4, 4), Mem::new(1, 4096, 4096, 4, 4));
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let ca = a.connect(0, 1111, SocketAddrV4::new(IP_B, 2222)).unwrap();
    let cb = b.connect(0, 2222, SocketAddrV4::new(IP_A, 1111)).unwrap();
    let (pat, mut buf) = (pattern(), vec![0u8; 4096]);
    let (mut fa, mut fb) = (Flow::default(), Flow::default());
    run(&mut link, &mut a, &mut b, 60 * SEC, |a, b, _| {
        let progress = pump(a, ca, &mut fa, 10_000, &pat, &mut buf)
            | pump(b, cb, &mut fb, 10_000, &pat, &mut buf);
        (
            progress,
            fa.eof && fb.eof && state(a, ca) == State::Closed && state(b, cb) == State::Closed,
        )
    });
    assert_eq!((fa.received, fb.received), (10_000, 10_000));
}

/// Polls `stack` at each deadline from `from` up to `to`, the peer answering ARP; returns what it sent, and when.
fn timers(stack: &mut Stack, tap: &mut Tap, from: u64, to: u64) -> Vec<(u64, Seg)> {
    let (mut now, mut sent) = (from, Vec::new());
    loop {
        sent.extend(feed(stack, tap, now, []).into_iter().map(|s| (now, s)));
        match stack.poll(tap, now) {
            Some(t) if t <= to => now = t.max(now + 1),
            _ => return sent,
        }
    }
}

#[test]
fn connect_gives_up_after_six_syn_retransmissions() {
    let mut m = Mem::new(1, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let c = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let sent = timers(&mut a, &mut tap, 0, 1000 * SEC);
    let at: Vec<u64> = sent
        .iter()
        .map(|(t, s)| {
            assert_eq!(s.flags, SYN);
            t / SEC
        })
        .collect();
    assert_eq!(
        at,
        [0, 1, 3, 7, 15, 31, 63],
        "RFC 6298 backoff from a 1 s initial RTO"
    );
    assert_eq!(a.recv(c, &mut [0; 8]), Err(Error::TimedOut));
}

#[test]
fn data_retransmission_gives_up_when_the_peer_vanishes() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, _) = accepted(&mut a, &mut tap);
    a.send(c, &[1; 3000]).unwrap();
    let sent = timers(&mut a, &mut tap, 0, 3600 * SEC);
    let retransmits = sent.iter().filter(|(t, _)| *t > 0).count();
    assert_eq!(retransmits, 10, "one segment per timeout, ten timeouts");
    assert_eq!(a.recv(c, &mut [0; 8]), Err(Error::TimedOut));
}

#[test]
fn a_half_open_entry_retries_its_syn_ack_then_expires() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    a.listen(PORT).unwrap();
    let p = Peer {
        port: PEER_PORT,
        to: PORT,
        seq: 1000,
        ack: 0,
    };
    let first = feed(&mut a, &mut tap, 0, [p.seg(1000, 0, SYN, 65535, &[], &[])]);
    let iss = first[0].seq;
    let sent = timers(&mut a, &mut tap, 0, 600 * SEC);
    let at: Vec<u64> = sent
        .iter()
        .map(|(t, s)| {
            assert_eq!((s.flags, s.seq), (SYN | ACK, iss));
            t / SEC
        })
        .collect();
    assert_eq!(at, [1, 3, 7, 15, 31]);
    let late = feed(
        &mut a,
        &mut tap,
        600 * SEC,
        [p.seg(1001, iss + 1, ACK, 65535, &[], &[])],
    );
    assert_eq!(late[0].flags, RST, "the entry is gone");
}

#[test]
fn fin_wait_2_and_time_wait_time_out() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.tcp_close(c);
    let fin = feed(&mut a, &mut tap, 0, []);
    assert_eq!(fin[0].flags, FIN | ACK);
    p.ack += 1;
    feed(&mut a, &mut tap, 0, [p.now(ACK, &[])]);
    let to = SocketAddrV4::new(IP_B, PEER_PORT);
    assert_eq!(
        a.connect(0, 0, to),
        Err(Error::TableFull),
        "FIN-WAIT-2 holds the slot"
    );
    timers(&mut a, &mut tap, 0, 61 * SEC);
    assert!(
        a.connect(61 * SEC, 0, to).is_ok(),
        "freed after 60 s without the peer's FIN"
    );

    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.tcp_close(c);
    feed(&mut a, &mut tap, 0, []);
    p.ack += 1;
    let ack = feed(&mut a, &mut tap, 0, [p.now(FIN | ACK, &[])]);
    assert_eq!((ack[0].flags, ack[0].ack), (ACK, p.seq + 1));
    let again = feed(&mut a, &mut tap, 30 * SEC, [p.now(FIN | ACK, &[])]);
    assert_eq!(
        (again[0].flags, again[0].ack),
        (ACK, p.seq + 1),
        "TIME_WAIT answers a resent FIN"
    );
    let bogus = p.seg(p.seq + 5, p.ack, FIN | ACK, 65535, &[], &[]);
    assert_eq!(feed(&mut a, &mut tap, 50 * SEC, [bogus])[0].flags, ACK);
    let gone = feed(&mut a, &mut tap, 91 * SEC, [p.now(FIN | ACK, &[])]);
    assert_eq!(
        gone[0].flags, RST,
        "60 s after the last FIN, TIME_WAIT is over"
    );
}

#[test]
fn sequential_connections_outnumber_the_table_by_reusing_time_wait() {
    let mut link = Link::new(1, faults(0), [MAC_A, MAC_B]);
    let (mut ma, mut mb) = (Mem::new(1, 4096, 4096, 4, 4), Mem::new(2, 4096, 4096, 4, 4));
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let listener = b.listen(PORT).unwrap();
    let mut buf = [0u8; 64];
    let mut get = |a: &mut Stack, b: &mut Stack, link: &mut Link, port: u16| {
        let ca = a
            .connect(link.now, port, SocketAddrV4::new(IP_B, PORT))
            .unwrap();
        a.send(ca, b"GET").unwrap();
        let (mut cb, mut answered, mut body) = (None, false, Vec::new());
        run(link, a, b, link.now + SEC, |a, b, _| {
            cb = cb.or_else(|| b.accept(listener));
            let mut progress = false;
            if let Some(cb) = cb.filter(|_| !answered)
                && let Ok(3) = b.recv(cb, &mut buf)
            {
                b.send(cb, b"page").unwrap();
                b.tcp_close(cb);
                (answered, progress) = (true, true);
            }
            match a.recv(ca, &mut buf) {
                Ok(0) if state(a, ca) == State::CloseWait => {
                    a.tcp_close(ca);
                    progress = true;
                }
                Ok(n) => body.extend_from_slice(&buf[..n]),
                _ => {}
            }
            // B closed first: its connection ends in TIME_WAIT, A's when its FIN is acknowledged.
            let done =
                state(a, ca) == State::Closed && cb.is_some_and(|cb| state(b, cb) == State::Closed);
            (progress, done)
        });
        assert_eq!(body, b"page", "connection from port {port}");
    };
    for i in 0..20 {
        get(&mut a, &mut b, &mut link, 5000 + i);
    }
    assert_eq!(
        b.counters.time_wait_reused, 16,
        "20 connections through 1 slot and 4 TIME_WAIT entries"
    );
    get(&mut a, &mut b, &mut link, 5019);
    assert_eq!(
        b.counters.time_wait_reused, 16,
        "a SYN above TIME_WAIT's sequence takes its entry over"
    );
}

/// A connects to B over a clean link with 1 ms delay; `$step` runs after each poll until it returns true.
macro_rules! pair {
    ($link:ident, $a:ident, $b:ident, $ca:ident, $cb:ident, $ma:ident, $mb:ident) => {
        let mut $link = Link::new(
            1,
            Faults {
                delay: MS,
                ..Faults::default()
            },
            [MAC_A, MAC_B],
        );
        let (mut $ma, mut $mb) = (
            Mem::new(1, 64 << 10, 64 << 10, 4, 4),
            Mem::new(2, 64 << 10, 64 << 10, 4, 4),
        );
        host!($a, $ma, IP_A, [1, 1]);
        host!($b, $mb, IP_B, [2, 2]);
        let listener = $b.listen(PORT).unwrap();
        let $ca = $a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
        let mut $cb = None;
        run(&mut $link, &mut $a, &mut $b, SEC, |a, b, _| {
            $cb = $cb.or_else(|| b.accept(listener));
            (false, $cb.is_some() && state(a, $ca) == State::Established)
        });
        let $cb = $cb.unwrap();
    };
}

#[test]
fn mutated_tcp_segments_never_panic_and_are_dropped_and_counted() {
    let bytes = 32 << 10;
    let (pat, mut buf) = (pattern(), vec![0u8; 64 << 10]);
    let recorded = {
        pair!(link, a, b, ca, cb, ma, mb);
        link.record = Some(Vec::new());
        // Neither side shuts down, so the segments are data and ACKs only.
        let (mut fa, mut fb) = (
            Flow {
                shut: true,
                ..Flow::default()
            },
            Flow {
                shut: true,
                ..Flow::default()
            },
        );
        run(&mut link, &mut a, &mut b, 60 * SEC, |a, b, _| {
            let progress = pump(a, ca, &mut fa, bytes, &pat, &mut buf)
                | pump(b, cb, &mut fb, bytes, &pat, &mut buf);
            (progress, fa.received == bytes && fb.received == bytes)
        });
        link.record.take().unwrap()
    };
    let to = |mac: Mac| -> Vec<Vec<u8>> {
        recorded
            .iter()
            .filter(|f| f[..6] == mac && parse(f).is_some())
            .cloned()
            .collect()
    };
    let (to_a, to_b) = (to(MAC_A), to(MAC_B));
    assert!(to_a.len() > 20 && to_b.len() > 20);

    // The same handshake again (same keys and times, so the same ISNs): the recorded segments fit it.
    pair!(link, a, b, ca, cb, ma, mb);
    let now = link.now;
    // Each end sends its data again as the recorded ACKs open its window.
    assert_eq!(
        (a.send(ca, &pat[..bytes]), b.send(cb, &pat[..bytes])),
        (Ok(bytes), Ok(bytes))
    );
    let (mut tap_a, mut tap_b) = (Tap::new(MAC_A), Tap::new(MAC_B));
    b.poll(&mut tap_b, now);
    tap_b.rx.push_back(arp_reply((MAC_A, IP_A), (MAC_B, IP_B)));
    b.poll(&mut tap_b, now);
    let (mut got_a, mut got_b) = (0, 0);
    let mut rng = Rng::new(42);
    for round in 0..100_000 {
        let side_a = rng.below(2) == 0;
        let frames = if side_a { &to_a } else { &to_b };
        let mut frame = frames[rng.below(frames.len() as u64) as usize].clone();
        let single = round < 50_000;
        let mut detectable = true;
        for _ in 0..if single { 1 } else { 1 + rng.below(8) } {
            if !frame.is_empty() {
                detectable &= mutate(&mut rng, &mut frame);
            }
        }
        if single && !detectable {
            continue;
        }
        let (stack, tap, id, got) = if side_a {
            (&mut a, &mut tap_a, ca, &mut got_a)
        } else {
            (&mut b, &mut tap_b, cb, &mut got_b)
        };
        let before = stack.counters;
        tap.rx.push_back(frame);
        tap.tx.clear();
        stack.poll(tap, now);
        let c = stack.counters;
        live(stack, id);
        assert_eq!(c.rx, before.rx + 1);
        assert_eq!(
            drops(&c) - drops(&before) + c.tcp - before.tcp,
            1,
            "round {round}: counted once"
        );
        while let Ok(n) = stack.recv(id, &mut buf) {
            if n == 0 {
                break;
            }
            let at = *got % PERIOD;
            assert!(
                !single || buf[..n] == pat[at..at + n],
                "round {round}: a single mutation changed data"
            );
            *got += n;
        }
    }
    assert!(
        got_a > bytes / 2 && got_b > bytes / 2,
        "recorded data was delivered: {got_a} {got_b}"
    );
    for c in [&a.counters, &b.counters] {
        assert!(
            c.checksum > 1000 && c.malformed > 100 && c.unacceptable > 1000 && c.tcp > 50,
            "{c:?}"
        );
    }
}

#[test]
fn full_tables_give_named_errors_and_a_handshake_waits_for_a_free_slot() {
    let mut link = Link::new(1, faults(0), [MAC_A, MAC_B]);
    let (mut ma, mut mb) = (Mem::new(2, 4096, 4096, 4, 4), Mem::new(2, 4096, 4096, 4, 4));
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let listener = b.listen(PORT).unwrap();
    assert_eq!(b.listen(PORT), Err(Error::InUse));
    assert_eq!(b.listen(0), Err(Error::Invalid));
    let to = SocketAddrV4::new(IP_B, PORT);
    let c1 = a.connect(0, 7000, to).unwrap();
    assert_eq!(a.connect(0, 7000, to), Err(Error::InUse));
    let c2 = a.connect(0, 7001, to).unwrap();
    assert_eq!(a.connect(0, 0, to), Err(Error::TableFull));
    assert_eq!(a.send(c2, b"hi"), Ok(2));
    let mut s1 = None;
    run(&mut link, &mut a, &mut b, 10 * SEC, |a, b, _| {
        s1 = s1.or_else(|| b.accept(listener));
        let up = state(a, c1) == State::Established && state(a, c2) == State::Established;
        (false, up && s1.is_some() && b.counters.socket_full > 0)
    });
    assert_eq!(b.accept(listener), None, "B's one connection slot is taken");
    b.abort(s1.unwrap());
    let mut s2 = None;
    let mut buf = [0u8; 8];
    run(&mut link, &mut a, &mut b, 60 * SEC, |_, b, _| {
        s2 = s2.or_else(|| b.accept(listener));
        (false, s2.is_some_and(|s| b.recv(s, &mut buf) == Ok(2)))
    });
    assert_eq!(
        &buf[..2],
        b"hi",
        "the retransmitted data completed the handshake"
    );
}

#[test]
fn a_window_update_in_fin_wait_2_does_not_pin_a_released_slot() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.tcp_close(c);
    feed(&mut a, &mut tap, 0, []);
    p.ack += 1;
    feed(&mut a, &mut tap, 0, [p.now(ACK, &[])]);
    assert_eq!(state(&a, c), State::FinWait2);
    feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, p.ack, ACK, 1234, &[], &[])],
    );
    timers(&mut a, &mut tap, 0, 3600 * SEC);
    let to = SocketAddrV4::new(IP_B, PEER_PORT);
    assert!(
        a.connect(61 * SEC, 0, to).is_ok(),
        "freed 60 s after the peer's last segment"
    );
}

#[test]
fn zero_window_probes_give_up_on_a_silent_peer() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    feed(&mut a, &mut tap, 0, [p.seg(p.seq, p.ack, ACK, 0, &[], &[])]);
    a.send(c, &[1; 1000]).unwrap();
    let probes = timers(&mut a, &mut tap, 0, 24 * 3600 * SEC);
    assert!((10..=12).contains(&probes.len()), "{} probes", probes.len());
    assert_eq!(a.recv(c, &mut [0; 8]), Err(Error::TimedOut));
}

#[test]
fn a_timeout_retransmits_even_into_a_window_below_one_mss() {
    let mut m = Mem::new(2, 4096, 64 << 10, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, p.ack, ACK, 500, &[], &[])],
    );
    a.send(c, &[1; 300]).unwrap();
    let first = feed(&mut a, &mut tap, 0, []);
    assert_eq!(first[0].data.len(), 300);
    a.send(c, &[2; 10_000]).unwrap();
    let sent = timers(&mut a, &mut tap, 1, 10 * SEC);
    assert!(
        sent.iter()
            .any(|(_, s)| s.seq == first[0].seq && s.data.len() >= 300),
        "the lost 300 bytes are resent"
    );
}

#[test]
fn a_pure_ack_during_go_back_n_carries_snd_max() {
    let mut m = Mem::new(2, 4096, 64 << 10, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    a.send(c, &[1; 3 * 1460]).unwrap();
    assert_eq!(feed(&mut a, &mut tap, 0, []).len(), 3);
    let resent = feed(&mut a, &mut tap, SEC, []);
    assert_eq!(resent[0].seq, p.ack, "the timeout goes back to snd_una");
    let ack = feed(&mut a, &mut tap, SEC, [p.now(ACK, b"hi")]);
    assert_eq!((ack[0].seq, ack[0].ack), (p.ack + 3 * 1460, p.seq + 2));
}

#[test]
fn time_wait_takeover_starts_above_the_old_sequence_space() {
    let mut link = Link::new(
        1,
        Faults {
            delay: MS,
            ..Faults::default()
        },
        [MAC_A, MAC_B],
    );
    let (mut ma, mut mb) = (
        Mem::new(1, 64 << 10, 64 << 10, 4, 4),
        Mem::new(2, 64 << 10, 64 << 10, 4, 4),
    );
    host!(a, ma, IP_A, [1, 1]);
    host!(b, mb, IP_B, [2, 2]);
    let listener = b.listen(PORT).unwrap();
    let ca = a.connect(0, 5555, SocketAddrV4::new(IP_B, PORT)).unwrap();
    link.record = Some(Vec::new());
    let bytes = 4 << 20;
    let (pat, mut buf) = (pattern(), vec![0u8; 64 << 10]);
    let (mut cb, mut fb, mut got, mut a_closed) = (None, Flow::default(), 0usize, false);
    run(&mut link, &mut a, &mut b, 600 * SEC, |a, b, _| {
        cb = cb.or_else(|| b.accept(listener));
        let mut progress = false;
        if let Some(cb) = cb {
            progress |= pump(b, cb, &mut fb, bytes, &pat, &mut buf);
        }
        loop {
            match a.recv(ca, &mut buf) {
                Ok(0) if !a_closed => {
                    a.tcp_close(ca);
                    (a_closed, progress) = (true, true);
                }
                Ok(n) if n > 0 => (got, progress) = (got + n, true),
                _ => break,
            }
        }
        let done = a_closed
            && state(a, ca) == State::Closed
            && cb.is_some_and(|c| state(b, c) == State::Closed);
        (progress, done)
    });
    b.tcp_close(cb.unwrap());
    let old = link.record.take().unwrap();
    let t1 = link.now;
    let ca2 = a.connect(t1, 5555, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let mut cb2 = None;
    run(&mut link, &mut a, &mut b, t1 + 10 * SEC, |a, b, _| {
        cb2 = cb2.or_else(|| b.accept(listener));
        (false, cb2.is_some() && state(a, ca2) == State::Established)
    });
    let mut tap = Tap::new(MAC_A);
    let old_data = old
        .iter()
        .filter(|f| f[..6] == MAC_A && parse(f).is_some_and(|s| !s.data.is_empty()));
    tap.rx.extend(old_data.cloned());
    a.poll(&mut tap, link.now);
    assert_eq!(
        a.recv(ca2, &mut buf),
        Err(Error::WouldBlock),
        "no old duplicate reaches the new connection"
    );
}

#[test]
fn out_of_window_segments_get_at_most_one_ack_per_half_second() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 2]);
    let mut tap = Tap::new(MAC_A);
    let (_, p) = accepted(&mut a, &mut tap);
    let blind = |i| {
        p.seg(
            p.seq.wrapping_add(1 << 30).wrapping_add(i),
            12345,
            ACK,
            65535,
            &[],
            &[],
        )
    };
    assert_eq!(feed(&mut a, &mut tap, 0, (0..100).map(blind)).len(), 1);
    assert_eq!(
        feed(&mut a, &mut tap, 400 * MS, (0..10).map(blind)).len(),
        0
    );
    assert_eq!(
        feed(&mut a, &mut tap, 500 * MS, (0..10).map(blind)).len(),
        1
    );
}

#[test]
fn a_silent_peer_always_ends_in_closed() {
    for seed in 0..40 {
        let mut link = Link::new(
            seed,
            Faults {
                arp_loss: if seed % 4 == 0 { 1000 } else { 300 },
                ..faults(10)
            },
            [MAC_A, MAC_B],
        );
        let (mut ma, mut mb) = (
            Mem::new(1, 16 << 10, 16 << 10, 4, 4),
            Mem::new(2, 16 << 10, 16 << 10, 4, 4),
        );
        host!(a, ma, IP_A, [seed, 1]);
        host!(b, mb, IP_B, [seed, 2]);
        let listener = b.listen(PORT).unwrap();
        let ca = a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).unwrap();
        let (pat, mut buf) = (pattern(), vec![0u8; 4096]);
        let (mut fa, mut fb, mut cb) = (Flow::default(), Flow::default(), None);
        let cut = Rng::new(seed).below(400) * MS;
        run(&mut link, &mut a, &mut b, 3600 * SEC, |a, b, now| {
            cb = cb.or_else(|| b.accept(listener));
            if now >= cut || (fa.eof && fb.eof) {
                return (false, true);
            }
            let mut progress = pump(a, ca, &mut fa, 256 << 10, &pat, &mut buf);
            if let Some(cb) = cb {
                progress |= pump(b, cb, &mut fb, 256 << 10, &pat, &mut buf);
            }
            (progress, false)
        });
        link.faults.loss = 1000;
        if seed % 2 == 1 {
            a.tcp_close(ca);
            cb.inspect(|&cb| b.tcp_close(cb));
        }
        let ends: Vec<_> = [Some(ca), cb].into_iter().flatten().collect();
        run(&mut link, &mut a, &mut b, cut + 1800 * SEC, |a, b, _| {
            live(a, ca);
            cb.inspect(|&cb| live(b, cb));
            let done = |s: &Stack, id| {
                let i = s.tcp_info(id).unwrap();
                i.state == State::Closed || i.deadline.is_none()
            };
            (
                false,
                done(a, ends[0]) && ends.get(1).is_none_or(|&cb| done(b, cb)),
            )
        });
    }
}

#[test]
fn receiver_silly_window_avoidance_holds_small_window_increases() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    let mut last = None;
    for chunk in [1460, 1460, 1176] {
        last = feed(&mut a, &mut tap, 0, [p.now(ACK, &vec![7; chunk])]).pop();
        p.seq += chunk as u32;
    }
    assert_eq!(last.unwrap().win, 0, "the ring is full");
    a.recv(c, &mut [0; 100]).unwrap();
    assert!(
        feed(&mut a, &mut tap, 0, []).is_empty(),
        "100 bytes are not worth a window update"
    );
    let probe = feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq - 1, p.ack, ACK, 65535, &[], &[])],
    );
    assert_eq!(
        probe[0].win, 0,
        "the window stays shut below min(MSS, ring / 2)"
    );
    a.recv(c, &mut [0; 2000]).unwrap();
    let update = feed(&mut a, &mut tap, 0, []);
    assert_eq!(update[0].win, 2100);
}

#[test]
fn a_syn_ack_offers_the_window_of_a_connection_slot() {
    let mut m = Mem::new(2, 8192, 4096, 4, 4);
    m.rx[0] = vec![0; 64];
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    a.listen(PORT).unwrap();
    let p = Peer {
        port: PEER_PORT,
        to: PORT,
        seq: 1000,
        ack: 0,
    };
    let synack = feed(&mut a, &mut tap, 0, [p.seg(1000, 0, SYN, 65535, &[], &[])]);
    assert_eq!(synack[0].win, 8192);
}

#[test]
fn a_simultaneous_open_offers_window_scaling_only_if_the_peer_did() {
    let mut m = Mem::new(1, 1 << 20, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    a.connect(0, PORT, SocketAddrV4::new(IP_B, PEER_PORT))
        .unwrap();
    let syn = feed(&mut a, &mut tap, 0, []);
    let p = Peer {
        port: PEER_PORT,
        to: PORT,
        seq: 1000,
        ack: 0,
    };
    feed(&mut a, &mut tap, 0, [p.seg(1000, 0, SYN, 65535, &[], &[])]);
    let synack = tap
        .tx
        .iter()
        .find(|f| parse(f).is_some_and(|s| s.flags == SYN | ACK))
        .unwrap();
    assert_eq!(parse(synack).unwrap().seq, syn[0].seq);
    assert_eq!(
        &synack[34 + 24..34 + 28],
        &[1, 1, 1, 1],
        "no window-scale option"
    );
    assert_eq!(parse(synack).unwrap().win, 0xffff);
}

#[test]
fn an_ack_below_snd_una_never_updates_the_window() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    a.send(c, &[1; 1000]).unwrap();
    feed(&mut a, &mut tap, 0, []);
    feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, p.ack + 1000, ACK, 40000, &[], &[])],
    );
    feed(
        &mut a,
        &mut tap,
        0,
        [p.seg(p.seq, p.ack, ACK, 0, &[], b"x")],
    );
    assert_eq!(a.tcp_info(c).unwrap().snd_wnd, 40000);
}

/// The scripted peer opens a connection from `port` on a listener whose half-open table is full; returns the
/// cookie SYN-ACK.
fn cookie_syn(a: &mut Stack, tap: &mut Tap, now: u64, port: u16) -> Seg {
    let p = Peer {
        port,
        to: PORT,
        seq: 7000,
        ack: 0,
    };
    let synack = feed(
        a,
        tap,
        now,
        [p.seg(7000, 0, SYN, 65535, &[2, 4, 5, 0xb4, 1, 3, 3, 7], &[])],
    );
    assert_eq!(synack[0].flags, SYN | ACK);
    synack.into_iter().next().unwrap()
}

#[test]
fn syn_cookies_take_over_when_the_half_open_table_is_full() {
    let mut m = Mem::new(4, 4096, 4096, 1, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let listener = a.listen(PORT).unwrap();
    let real = Peer {
        port: PEER_PORT,
        to: PORT,
        seq: 1000,
        ack: 0,
    };
    let iss = feed(
        &mut a,
        &mut tap,
        0,
        [real.seg(1000, 0, SYN, 65535, &[], &[])],
    )[0]
    .seq;
    // A flood inside the real client's round trip no longer evicts it.
    let flood = (0..8).map(|i| {
        Peer {
            port: 2000 + i,
            ..real
        }
        .seg(77, 0, SYN, 65535, &[], &[])
    });
    assert_eq!(feed(&mut a, &mut tap, MS, flood).len(), 8);
    assert_eq!(a.counters.syn_cookies, 8);
    assert!(
        feed(
            &mut a,
            &mut tap,
            2 * MS,
            [real.seg(1001, iss + 1, ACK, 65535, &[], &[])]
        )
        .is_empty()
    );
    assert!(a.accept(listener).is_some());

    feed(
        &mut a,
        &mut tap,
        3 * MS,
        [Peer { port: 2999, ..real }.seg(5, 0, SYN, 65535, &[], &[])],
    );
    let cookie = cookie_syn(&mut a, &mut tap, 3 * MS, 3000);
    let q = Peer {
        port: 3000,
        to: PORT,
        seq: 7001,
        ack: cookie.seq + 1,
    };
    assert_eq!(
        &tap.tx[0][34 + 20..34 + 24],
        &[2, 4, 5, 0xb4],
        "the MSS survives in the cookie"
    );
    assert_eq!(
        &tap.tx[0][34 + 24..34 + 28],
        &[1, 1, 1, 1],
        "no window scaling without timestamps"
    );
    let bad = a.counters.bad_cookies;
    let forged = q.seg(7001, cookie.seq + 2, ACK, 65535, &[], &[]);
    assert_eq!(feed(&mut a, &mut tap, 3 * MS, [forged])[0].flags, RST);
    let wrong_isn = q.seg(7002, cookie.seq + 1, ACK, 65535, &[], &[]);
    assert_eq!(feed(&mut a, &mut tap, 3 * MS, [wrong_isn])[0].flags, RST);
    assert_eq!(a.counters.bad_cookies, bad + 2);
    assert!(feed(&mut a, &mut tap, 4 * MS, [q.now(ACK, b"GET")]).len() == 1);
    let c = a
        .accept(listener)
        .expect("a valid cookie opens the connection");
    assert_eq!(a.recv(c, &mut [0; 8]), Ok(3));
    feed(&mut a, &mut tap, 5 * MS, [q.now(ACK, b"GET")]);
    assert_eq!(
        a.accept(listener),
        None,
        "a replayed cookie ACK reaches the open connection, not a new one"
    );

    let late = cookie_syn(&mut a, &mut tap, 10 * MS, 3001);
    let r = Peer {
        port: 3001,
        to: PORT,
        seq: 7001,
        ack: late.seq + 1,
    };
    assert_eq!(
        feed(&mut a, &mut tap, 10 * MS + 40 * SEC, [r.now(ACK, &[])])[0].flags,
        RST
    );
    assert_eq!(
        a.counters.bad_cookies,
        bad + 3,
        "an expired cookie is rejected"
    );
    assert_eq!(a.accept(listener), None);
}

#[test]
fn a_valid_cookie_opens_nothing_on_a_listener_that_sent_no_cookies() {
    // Twin stacks with the same key and address compute the same cookie; only `flooded` has sent any.
    let (mut mf, mut mq) = (Mem::new(2, 4096, 4096, 0, 4), Mem::new(2, 4096, 4096, 4, 4));
    host!(flooded, mf, IP_A, [9, 9]);
    host!(quiet, mq, IP_A, [9, 9]);
    let (lf, lq) = (flooded.listen(PORT).unwrap(), quiet.listen(PORT).unwrap());
    let mut tap = Tap::new(MAC_A);
    let cookie = cookie_syn(&mut flooded, &mut tap, 0, 3000);
    let q = Peer {
        port: 3000,
        to: PORT,
        seq: 7001,
        ack: cookie.seq + 1,
    };
    let out = feed(&mut quiet, &mut tap, MS, [q.now(ACK, &[])]);
    assert_eq!(
        out[0].flags, RST,
        "a listener that sent no cookie accepts none"
    );
    assert_eq!((quiet.accept(lq), quiet.counters.bad_cookies), (None, 1));

    let out = feed(&mut flooded, &mut tap, MS, [q.now(SYN | ACK, &[])]);
    assert_eq!(out[0].flags, RST, "a SYN-ACK is never a cookie's ACK");
    assert_eq!(flooded.accept(lf), None);
    feed(&mut flooded, &mut tap, MS, [q.now(ACK, &[])]);
    assert!(flooded.accept(lf).is_some(), "the cookie itself is good");
}

#[test]
fn a_time_wait_takeover_iss_is_unpredictable() {
    let mut gaps = Vec::new();
    for key in [[1, 1], [2, 2]] {
        let mut m = Mem::new(2, 4096, 4096, 4, 4);
        host!(a, m, IP_A, key);
        let mut tap = Tap::new(MAC_A);
        let (c, mut p) = accepted(&mut a, &mut tap);
        a.tcp_close(c);
        feed(&mut a, &mut tap, 0, []);
        p.ack += 1;
        feed(&mut a, &mut tap, 0, [p.now(FIN | ACK, &[])]);
        let synack = feed(
            &mut a,
            &mut tap,
            SEC,
            [p.seg(p.seq + 10, 0, SYN, 65535, &[], &[])],
        );
        assert_eq!(synack[0].flags, SYN | ACK, "the SYN takes TIME_WAIT over");
        gaps.push(synack[0].seq.wrapping_sub(p.ack));
    }
    assert!(
        gaps.iter()
            .all(|&g| (65537..65537 + (1 << 24)).contains(&g)),
        "{gaps:?}"
    );
    assert_ne!(
        gaps[0], gaps[1],
        "the new ISS is keyed, not old snd_nxt + a constant"
    );
}

#[test]
fn an_open_half_closed_connection_waits_for_a_slow_peer() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.shutdown(c);
    assert_eq!(feed(&mut a, &mut tap, 0, [])[0].flags, FIN | ACK);
    p.ack += 1;
    feed(&mut a, &mut tap, 0, [p.now(ACK, &[])]);
    assert_eq!(state(&a, c), State::FinWait2);
    timers(&mut a, &mut tap, 0, 120 * SEC);
    feed(&mut a, &mut tap, 120 * SEC, [p.now(FIN | ACK, b"late")]);
    let mut buf = [0u8; 8];
    assert_eq!(
        a.recv(c, &mut buf),
        Ok(4),
        "the answer arrives after two minutes"
    );
    assert_eq!(a.recv(c, &mut buf), Ok(0));
}

#[test]
fn closing_a_connection_already_in_fin_wait_2_starts_its_timeout() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.shutdown(c);
    feed(&mut a, &mut tap, 0, []);
    p.ack += 1;
    feed(&mut a, &mut tap, 0, [p.now(ACK, &[])]);
    timers(&mut a, &mut tap, 0, 100 * SEC);
    a.tcp_close(c);
    timers(&mut a, &mut tap, 100 * SEC, 3600 * SEC);
    assert!(
        a.connect(3600 * SEC, 0, SocketAddrV4::new(IP_B, PEER_PORT))
            .is_ok(),
        "released and freed"
    );
}

/// Polls `stack` with nobody answering ARP, following its deadlines up to `to`; returns when it stopped.
fn polls_no_arp(stack: &mut Stack, tap: &mut Tap, from: u64, to: u64) -> u64 {
    let mut now = from;
    loop {
        tap.tx.clear();
        match stack.poll(tap, now) {
            Some(t) if t <= to => now = t.max(now + 1),
            _ => return now,
        }
    }
}

#[test]
fn a_fin_that_never_leaves_still_has_a_deadline() {
    for close_wait in [false, true] {
        let mut m = Mem::new(2, 4096, 4096, 4, 4);
        host!(a, m, IP_A, [1, 1]);
        let mut tap = Tap::new(MAC_A);
        let (c, p) = accepted(&mut a, &mut tap);
        if close_wait {
            tap.rx.push_back(p.now(ACK | FIN, &[]));
            a.poll(&mut tap, 0);
        }
        a.tcp_close(c);
        a.poll(&mut tap, 0);
        live(&a, c);
        let end = polls_no_arp(&mut a, &mut tap, 0, 24 * 3600 * SEC);
        assert_eq!(
            state(&a, c),
            State::Closed,
            "next hop never resolved: the owed FIN times out"
        );
        assert!(
            a.connect(end, 0, SocketAddrV4::new(IP_B, 1)).is_ok(),
            "the slot is freed"
        );
    }
}

#[test]
fn a_released_connection_gives_up_on_a_peer_that_keeps_its_window_shut() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, p) = accepted(&mut a, &mut tap);
    feed(&mut a, &mut tap, 0, [p.seg(p.seq, p.ack, ACK, 0, &[], &[])]);
    a.send(c, &[1; 1000]).unwrap();
    a.tcp_close(c);
    let mut now = 0;
    while let Some(t) = a.poll(&mut tap, now) {
        now = t.max(now + 1);
        if !feed(&mut a, &mut tap, now, []).is_empty() {
            feed(
                &mut a,
                &mut tap,
                now,
                [p.seg(p.seq, p.ack, ACK, 0, &[], &[])],
            );
        }
        assert!(now < 3600 * SEC, "still probing after an hour");
    }
    assert_eq!(state(&a, c), State::Closed);
}

#[test]
fn a_released_fin_wait_2_is_not_kept_alive_by_the_peers_acks() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let (c, mut p) = accepted(&mut a, &mut tap);
    a.tcp_close(c);
    feed(&mut a, &mut tap, 0, []);
    p.ack += 1;
    feed(&mut a, &mut tap, 0, [p.now(ACK, &[])]);
    assert!(a.tcp_info(c).unwrap().released);
    for i in 1..=3 {
        feed(&mut a, &mut tap, i * 30 * SEC, [p.now(ACK, &[])]);
    }
    assert_eq!(
        state(&a, c),
        State::Closed,
        "60 s after the FIN was acknowledged, whatever the peer sends"
    );
}

/// A connection from port 5555 to the peer's PORT, closed by us first so it sits in TIME_WAIT; returns the peer.
fn in_time_wait(a: &mut Stack, tap: &mut Tap) -> Peer {
    let c = a.connect(0, 5555, SocketAddrV4::new(IP_B, PORT)).unwrap();
    let syn = feed(a, tap, 0, [])
        .into_iter()
        .find(|s| s.flags == SYN)
        .unwrap();
    let mut p = Peer {
        port: PORT,
        to: 5555,
        seq: 7000,
        ack: syn.seq.wrapping_add(1),
    };
    feed(
        a,
        tap,
        MS,
        [p.seg(7000, p.ack, SYN | ACK, 65535, &[2, 4, 5, 0xb4], &[])],
    );
    p.seq = 7001;
    a.tcp_close(c);
    feed(a, tap, 2 * MS, []);
    p.ack = p.ack.wrapping_add(1);
    feed(a, tap, 3 * MS, [p.now(ACK | FIN, &[])]);
    p.seq += 1;
    assert_eq!(state(a, c), State::Closed);
    p
}

#[test]
fn a_syn_on_time_wait_without_a_listener_keeps_the_entry() {
    let mut m = Mem::new(2, 4096, 4096, 4, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let p = in_time_wait(&mut a, &mut tap);
    let syn = feed(
        &mut a,
        &mut tap,
        5 * MS,
        [p.seg(p.seq + 100, 0, SYN, 65535, &[], &[])],
    );
    assert_eq!(syn[0].flags, ACK, "TIME_WAIT answers the SYN");
    let fin = feed(
        &mut a,
        &mut tap,
        6 * MS,
        [p.seg(p.seq - 1, p.ack, ACK | FIN, 65535, &[], &[])],
    );
    assert_eq!(
        fin[0].flags, ACK,
        "and is still there for the retransmitted FIN"
    );
}

#[test]
fn a_time_wait_takeover_never_falls_back_to_a_cookie() {
    let mut m = Mem::new(3, 4096, 4096, 1, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let p = in_time_wait(&mut a, &mut tap);
    let l = a.listen(5555).unwrap();
    let spoof = Peer { port: 2000, ..p };
    feed(
        &mut a,
        &mut tap,
        4 * MS,
        [spoof.seg(77, 0, SYN, 65535, &[], &[])],
    );
    let out = feed(
        &mut a,
        &mut tap,
        5 * MS,
        [p.seg(p.seq + 100, 0, SYN, 65535, &[], &[])],
    );
    assert_eq!(
        (out[0].flags, a.counters.syn_cookies),
        (ACK, 0),
        "no room for the gap ISS: TIME_WAIT answers"
    );
    feed(
        &mut a,
        &mut tap,
        6 * MS,
        [spoof.seg(78, 0, RST, 0, &[], &[])],
    );
    let out = feed(
        &mut a,
        &mut tap,
        7 * MS,
        [p.seg(p.seq + 100, 0, SYN, 65535, &[], &[])],
    );
    assert_eq!(
        out[0].flags,
        SYN | ACK,
        "with room, the SYN takes TIME_WAIT over"
    );
    let _ = l;
}

#[test]
fn a_cookie_ack_is_accepted_even_after_the_syn_got_a_table_entry() {
    let mut m = Mem::new(3, 4096, 4096, 1, 4);
    host!(a, m, IP_A, [1, 1]);
    let mut tap = Tap::new(MAC_A);
    let l = a.listen(PORT).unwrap();
    let y = Peer {
        port: 2000,
        to: PORT,
        seq: 77,
        ack: 0,
    };
    feed(&mut a, &mut tap, 0, [y.seg(77, 0, SYN, 65535, &[], &[])]);
    let x = Peer {
        port: 3000,
        to: PORT,
        seq: 7000,
        ack: 0,
    };
    let cookie = feed(&mut a, &mut tap, MS, [x.seg(7000, 0, SYN, 65535, &[], &[])])[0].seq;
    assert_eq!(a.counters.syn_cookies, 1);
    feed(&mut a, &mut tap, 2 * MS, [y.seg(78, 0, RST, 0, &[], &[])]);
    let entry = feed(
        &mut a,
        &mut tap,
        3 * MS,
        [x.seg(7000, 0, SYN, 65535, &[], &[])],
    )[0]
    .seq;
    assert_ne!(entry, cookie);
    let out = feed(
        &mut a,
        &mut tap,
        4 * MS,
        [x.seg(7001, cookie + 1, ACK, 65535, &[], &[])],
    );
    assert!(out.iter().all(|s| s.flags & RST == 0));
    assert!(
        a.accept(l).is_some(),
        "the client answered the cookie SYN-ACK"
    );
}

#[test]
fn a_cookie_is_accepted_only_by_a_listener_that_sent_cookies() {
    let (mut mf, mut mq) = (Mem::new(4, 4096, 4096, 0, 4), Mem::new(4, 4096, 4096, 0, 4));
    host!(flooded, mf, IP_A, [9, 9]);
    host!(twin, mq, IP_A, [9, 9]);
    let other = flooded.listen(PORT + 1).unwrap();
    flooded.listen(PORT).unwrap();
    twin.listen(PORT + 1).unwrap();
    let mut tap = Tap::new(MAC_A);
    cookie_syn(&mut flooded, &mut tap, 0, 4000);
    let syn = Peer {
        port: 3000,
        to: PORT + 1,
        seq: 7000,
        ack: 0,
    };
    let cookie = feed(&mut twin, &mut tap, 0, [syn.now(SYN, &[])]).remove(0);
    assert_eq!((cookie.flags, twin.counters.syn_cookies), (SYN | ACK, 1));
    let q = Peer {
        port: 3000,
        to: PORT + 1,
        seq: 7001,
        ack: cookie.seq + 1,
    };
    let out = feed(&mut flooded, &mut tap, MS, [q.now(ACK, &[])]);
    assert_eq!(
        out[0].flags, RST,
        "a flood on port 80 does not open guessing on port 81"
    );
    assert_eq!(flooded.accept(other), None);
}

#[test]
fn a_cookie_is_accepted_in_its_period_and_the_next_only() {
    let p16 = 16 * SEC;
    for (issued, checked, want) in [
        (0, p16 - 1, true),
        (p16 - 1, p16, true),
        (p16 - 1, 2 * p16 - 1, true),
        (p16 - 1, 2 * p16, false),
        (p16, 3 * p16 - 1, true),
        (p16, 3 * p16, false),
        (5 * p16 + 3, 7 * p16 + 3, false),
    ] {
        let mut m = Mem::new(3, 4096, 4096, 0, 4);
        host!(a, m, IP_A, [7, 7]);
        let l = a.listen(PORT).unwrap();
        let mut tap = Tap::new(MAC_A);
        let p = Peer {
            port: 3000,
            to: PORT,
            seq: 7000,
            ack: 0,
        };
        let sa = feed(
            &mut a,
            &mut tap,
            issued,
            [p.seg(7000, 0, SYN, 65535, &[2, 4, 5, 0xb4], &[])],
        );
        // Keep the gate open so only the cookie's own clock decides.
        feed(
            &mut a,
            &mut tap,
            checked,
            [Peer { port: 9, ..p }.seg(1, 0, SYN, 65535, &[], &[])],
        );
        feed(
            &mut a,
            &mut tap,
            checked,
            [p.seg(7001, sa[0].seq.wrapping_add(1), ACK, 65535, &[], &[])],
        );
        assert_eq!(
            a.accept(l).is_some(),
            want,
            "issued {issued} checked {checked}"
        );
    }
}
