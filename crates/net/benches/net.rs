use std::hint::black_box;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Instant;

use net::{Config, Mac, Neighbor, Nic, Proto, Socket, Stack};

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

fn main() {
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
