use std::hint::black_box;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Instant;

use net::{
    Config, HalfOpen, Mac, Neighbor, Nic, Proto, Socket, Stack, State, Tcp, TcpId, TcpSocket,
    TimeWait,
};

#[path = "../tests/sim/mod.rs"]
mod sim;

const RUNS: usize = 21;
const DATAGRAMS: usize = 100_000;
const BATCH: usize = 16;
const MAC_A: Mac = [2, 0, 0, 0, 0, 1];
const MAC_B: Mac = [2, 0, 0, 0, 0, 2];
const IP_A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

fn config(ip: Ipv4Addr) -> Config {
    Config {
        ip,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: None,
    }
}

fn report(name: &str, mut ns: Vec<f64>) {
    ns.sort_by(f64::total_cmp);
    let (min, median) = (ns[0], ns[RUNS / 2]);
    println!(
        "{name}: min {min:.1} ns, median {median:.1} ns, {:.2} M/s at the median ({RUNS} runs)",
        1e3 / median
    );
}

/// Hands out the same frame `left` more times.
struct Repeat<'f> {
    frame: &'f [u8],
    left: usize,
}

impl Nic for Repeat<'_> {
    fn mac(&self) -> Mac {
        MAC_B
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, _: usize, _: impl FnOnce(&mut [u8])) -> bool {
        false
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        if self.left == 0 {
            return false;
        }
        self.left -= 1;
        f(self.frame);
        true
    }
}

/// A sends `size`-byte UDP datagrams to B over the loss-free simulated link, B receives each one; ns per datagram.
fn udp_link(size: usize) -> f64 {
    let mut link = sim::Link::new(1, sim::Faults::default(), [MAC_A, MAC_B]);
    let (mut na, mut nb) = ([Neighbor::EMPTY; 4], [Neighbor::EMPTY; 4]);
    let (mut bufa, mut bufb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    let mut sa = [Socket::new(&mut bufa)];
    let mut sb = [Socket::new(&mut bufb)];
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let client = a.bind(Proto::Udp, 1000).unwrap();
    let server = b.bind(Proto::Udp, 7).unwrap();
    let to = SocketAddrV4::new(IP_B, 7);
    let data = vec![0x5a; size];
    let mut buf = [0u8; 2048];
    let _ = a.send_to(&mut link.end(0), 0, client, to, &data);
    b.poll(&mut link.end(1), 0);
    a.poll(&mut link.end(0), 0);
    let start = Instant::now();
    let mut got = 0;
    while got < DATAGRAMS {
        for _ in 0..BATCH {
            a.send_to(&mut link.end(0), 0, client, to, &data).unwrap();
        }
        b.poll(&mut link.end(1), 0);
        while let Some((_, n)) = b.recv_from(server, &mut buf) {
            black_box(&buf[..n]);
            got += 1;
        }
    }
    assert_eq!(got, DATAGRAMS);
    start.elapsed().as_nanos() as f64 / DATAGRAMS as f64
}

/// B's receive path for one UDP frame (parse, checksum, demux, copy into the socket and out again), in ns.
fn receive_path(size: usize) -> f64 {
    let mut link = sim::Link::new(1, sim::Faults::default(), [MAC_A, MAC_B]);
    let (mut na, mut nb) = ([Neighbor::EMPTY; 1], [Neighbor::EMPTY; 1]);
    let (mut bufa, mut bufb) = (vec![0u8; 4096], vec![0u8; 1 << 16]);
    let mut sa = [Socket::new(&mut bufa)];
    let mut sb = [Socket::new(&mut bufb)];
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let client = a.bind(Proto::Udp, 1000).unwrap();
    let server = b.bind(Proto::Udp, 7).unwrap();
    let to = SocketAddrV4::new(IP_B, 7);
    let _ = a.send_to(&mut link.end(0), 0, client, to, &[]);
    b.poll(&mut link.end(1), 0);
    a.poll(&mut link.end(0), 0);
    link.record = Some(Vec::new());
    a.send_to(&mut link.end(0), 0, client, to, &vec![0x5a; size])
        .unwrap();
    let frame = link.record.take().unwrap().pop().unwrap();
    let mut buf = [0u8; 2048];
    let rounds = DATAGRAMS / BATCH;
    let start = Instant::now();
    for _ in 0..rounds {
        b.poll(
            &mut Repeat {
                frame: &frame,
                left: BATCH,
            },
            0,
        );
        for _ in 0..BATCH {
            let (_, n) = b.recv_from(server, &mut buf).unwrap();
            black_box(&buf[..n]);
        }
    }
    assert_eq!(b.counters.rx, (rounds * BATCH) as u64 + 1);
    start.elapsed().as_nanos() as f64 / (rounds * BATCH) as f64
}

/// Binds `$stack` to a TCP stack at `$ip` with `$slots` connections of `$ring`-byte rings.
macro_rules! host {
    ($stack:ident, $ip:expr, $key:expr, $slots:expr, $ring:expr) => {
        let mut neighbors = [Neighbor::EMPTY; 4];
        let (mut rx, mut tx) = (vec![vec![0u8; $ring]; $slots], vec![vec![0u8; $ring]; $slots]);
        let mut socks: Vec<TcpSocket> =
            rx.iter_mut().zip(tx.iter_mut()).map(|(r, t)| TcpSocket::new(r, t)).collect();
        let (mut half_open, mut time_wait) = ([HalfOpen::EMPTY; 4], [TimeWait::EMPTY; 4]);
        let mut $stack = Stack::new(config($ip), &mut neighbors, &mut []).with_tcp(Tcp::new(
            $key,
            &mut socks,
            &mut half_open,
            &mut time_wait,
        ));
    };
}

/// Steps A and B over the link until `step` says done, jumping virtual time to the next event.
fn drive(
    link: &mut sim::Link,
    a: &mut Stack,
    b: &mut Stack,
    mut step: impl FnMut(&mut Stack, &mut Stack) -> (bool, bool),
) {
    loop {
        let now = link.now;
        let da = a.poll(&mut link.end(0), now);
        let db = b.poll(&mut link.end(1), now);
        let (progress, done) = step(a, b);
        if done {
            return;
        }
        if !progress {
            link.now = [da, db, link.next()]
                .into_iter()
                .flatten()
                .min()
                .unwrap()
                .max(now);
        }
    }
}

/// A connects to B; returns both ends once B has accepted.
fn connect(link: &mut sim::Link, a: &mut Stack, b: &mut Stack, listener: TcpId) -> (TcpId, TcpId) {
    let ca = a.connect(link.now, 0, SocketAddrV4::new(IP_B, 80)).unwrap();
    let mut cb = None;
    drive(link, a, b, |_, b| {
        cb = cb.or_else(|| b.accept(listener));
        (false, cb.is_some())
    });
    (ca, cb.unwrap())
}

/// A sends `bytes` to B, which reads as it goes; returns the wall time in ns and the virtual time it took.
fn tcp_transfer(faults: sim::Faults, seed: u64, ring: usize, bytes: usize) -> (f64, u64) {
    let mut link = sim::Link::new(seed, faults, [MAC_A, MAC_B]);
    host!(a, IP_A, [seed, 1], 1, ring);
    host!(b, IP_B, [seed, 2], 2, ring);
    let listener = b.listen(80).unwrap();
    let (ca, cb) = connect(&mut link, &mut a, &mut b, listener);
    let (data, mut buf) = (vec![0x5a; 1 << 16], vec![0u8; 1 << 16]);
    let (mut sent, mut got, start, at) = (0, 0, Instant::now(), link.now);
    drive(&mut link, &mut a, &mut b, |a, b| {
        let mut progress = false;
        while sent < bytes
            && let Ok(n) = a.send(ca, &data[..(bytes - sent).min(data.len())])
        {
            (sent, progress) = (sent + n, true);
        }
        while let Ok(n) = b.recv(cb, &mut buf) {
            black_box(&buf[..n]);
            (got, progress) = (got + n, true);
        }
        (progress, got == bytes)
    });
    (start.elapsed().as_nanos() as f64, link.now - at)
}

/// Hands out recorded frames in turn; transmitted frames are built and dropped.
struct Replay<'f> {
    frames: &'f [Vec<u8>],
    next: usize,
    scratch: Vec<u8>,
}

impl Nic for Replay<'_> {
    fn mac(&self) -> Mac {
        MAC_B
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        fill(&mut self.scratch[..len]);
        black_box(&self.scratch);
        true
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        let Some(frame) = self.frames.get(self.next) else {
            return false;
        };
        self.next += 1;
        f(frame);
        true
    }
}

/// B's receive path per TCP data segment (nearly all 1460 bytes) (checksum, demux, sequence checks, copy into the ring, the ACK, and the
/// copy out with `recv`): A's segments recorded from a transfer, replayed into the same connection.
fn tcp_receive_path(frames: &[Vec<u8>]) -> f64 {
    let mut link = sim::Link::new(1, sim::Faults::default(), [MAC_A, MAC_B]);
    host!(a, IP_A, [1, 1], 1, 1 << 16);
    host!(b, IP_B, [1, 2], 2, 1 << 16);
    let listener = b.listen(80).unwrap();
    let (_, cb) = connect(&mut link, &mut a, &mut b, listener);
    let mut buf = vec![0u8; 1 << 16];
    let mut nic = Replay {
        frames: &[],
        next: 0,
        scratch: vec![0; 2048],
    };
    let taken = b.counters.tcp;
    let start = Instant::now();
    for batch in frames.chunks(BATCH) {
        (nic.frames, nic.next) = (batch, 0);
        b.poll(&mut nic, link.now);
        while let Ok(n) = b.recv(cb, &mut buf) {
            black_box(&buf[..n]);
        }
    }
    let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
    assert_eq!(
        b.counters.tcp - taken,
        frames.len() as u64,
        "every segment taken"
    );
    ns
}

/// A's data segments from a 4 MiB transfer over the loss-free link.
fn record_segments() -> Vec<Vec<u8>> {
    let mut link = sim::Link::new(1, sim::Faults::default(), [MAC_A, MAC_B]);
    host!(a, IP_A, [1, 1], 1, 1 << 16);
    host!(b, IP_B, [1, 2], 2, 1 << 16);
    let listener = b.listen(80).unwrap();
    let (ca, cb) = connect(&mut link, &mut a, &mut b, listener);
    link.record = Some(Vec::new());
    let (data, mut buf, bytes) = (vec![0x5a; 1 << 16], vec![0u8; 1 << 16], 4 << 20);
    let (mut sent, mut got) = (0, 0);
    drive(&mut link, &mut a, &mut b, |a, b| {
        let mut progress = false;
        while sent < bytes
            && let Ok(n) = a.send(ca, &data[..(bytes - sent).min(data.len())])
        {
            (sent, progress) = (sent + n, true);
        }
        while let Ok(n) = b.recv(cb, &mut buf) {
            (got, progress) = (got + n, true);
        }
        (progress, got == bytes)
    });
    let frames: Vec<_> = link
        .record
        .take()
        .unwrap()
        .into_iter()
        .filter(|f| f[..6] == MAC_B && f.len() > 60)
        .collect();
    assert!(frames.len() >= (4 << 20) / 1460);
    frames
}

/// Connect, accept, a close from each side and the TIME_WAIT entry, over the loss-free link; ns per connection.
fn handshake_and_close() -> f64 {
    let mut link = sim::Link::new(1, sim::Faults::default(), [MAC_A, MAC_B]);
    host!(a, IP_A, [1, 1], 1, 4096);
    host!(b, IP_B, [1, 2], 2, 4096);
    let listener = b.listen(80).unwrap();
    let n = 10_000;
    let mut start = Instant::now();
    for i in 0..=n {
        // The first connection also resolves ARP, so it is not timed.
        if i == 1 {
            start = Instant::now();
        }
        let (ca, cb) = connect(&mut link, &mut a, &mut b, listener);
        a.tcp_close(ca);
        drive(&mut link, &mut a, &mut b, |a, b| {
            if b.tcp_info(cb).unwrap().state == State::CloseWait {
                b.tcp_close(cb);
            }
            let closed = |s: &Stack, id| s.tcp_info(id).unwrap().state == State::Closed;
            (false, closed(a, ca) && closed(b, cb))
        });
    }
    start.elapsed().as_nanos() as f64 / n as f64
}

fn main() {
    for ring in [64 << 10, 1 << 20] {
        let bytes = 64 << 20;
        let mut mibs: Vec<f64> = (0..RUNS)
            .map(|_| {
                bytes as f64
                    / (1 << 20) as f64
                    / (tcp_transfer(sim::Faults::default(), 1, ring, bytes).0 / 1e9)
            })
            .collect();
        mibs.sort_by(f64::total_cmp);
        println!(
            "tcp goodput over the loss-free link, {} KiB window: min {:.0}, median {:.0} MiB/s ({RUNS} runs)",
            ring >> 10,
            mibs[0],
            mibs[RUNS / 2]
        );
    }
    let frames = record_segments();
    report(
        "tcp receive path per data segment",
        (0..RUNS).map(|_| tcp_receive_path(&frames)).collect(),
    );
    report(
        "tcp connect + accept + close both ways",
        (0..RUNS).map(|_| handshake_and_close()).collect(),
    );
    for (loss, delay) in [(10, 5), (10, 25), (50, 5), (50, 25)] {
        let faults = sim::Faults {
            loss,
            delay: delay * 1_000_000,
            ..sim::Faults::default()
        };
        let bytes = 16 << 20;
        let mut mibs: Vec<f64> = (0..11)
            .map(|seed| {
                bytes as f64
                    / (1 << 20) as f64
                    / (tcp_transfer(faults, seed, 1 << 20, bytes).1 as f64 / 1e9)
            })
            .collect();
        mibs.sort_by(f64::total_cmp);
        println!(
            "tcp simulated goodput, {}% loss, {} ms RTT, 1 MiB window: min {:.2}, median {:.2} MiB/s (11 seeds)",
            loss / 10,
            2 * delay,
            mibs[0],
            mibs[5]
        );
    }
    for size in [64, 1472] {
        report(
            &format!("udp over the simulated link, {size}-byte datagrams"),
            (0..RUNS).map(|_| udp_link(size)).collect(),
        );
        report(
            &format!("udp receive path, {size}-byte datagrams"),
            (0..RUNS).map(|_| receive_path(size)).collect(),
        );
    }
}
