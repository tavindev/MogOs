mod sim;

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};

use net::{Config, Counters, Error, Mac, Neighbor, Proto, Socket, Stack};
use sim::{Faults, Link, Rng, Tap};

const MAC_A: Mac = [2, 0, 0, 0, 0, 1];
const MAC_B: Mac = [2, 0, 0, 0, 0, 2];
const EVIL: Mac = [2, 0, 0, 0, 0, 0x66];
const IP_A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const ICMP_ID: u16 = 7;
const ECHO_PORT: u16 = 7;
const CLIENT_PORT: u16 = 1000;
const ITEMS: usize = 32;

fn config(ip: Ipv4Addr) -> Config {
    Config {
        ip,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: None,
    }
}

fn drops(c: &Counters) -> u64 {
    c.malformed + c.checksum + c.fragments + c.ignored + c.no_socket + c.socket_full
}

fn echo_request(seq: u16) -> Vec<u8> {
    let mut m = vec![8, 0, 0, 0, 0, 0];
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend((0..32).map(|i| i as u8 ^ seq as u8));
    m
}

fn datagram(i: usize) -> Vec<u8> {
    (0..20 + i * 97 % 1400)
        .map(|j| if j == 0 { i as u8 } else { (j * 7 + i) as u8 })
        .collect()
}

/// A pings B and sends UDP datagrams to B's echo server until every one has come back intact, or `limit` passes.
/// Returns the payloads A's sockets received.
fn exchange(link: &mut Link, limit: u64) -> Vec<Vec<u8>> {
    let (mut na, mut nb) = ([Neighbor::EMPTY; 4], [Neighbor::EMPTY; 4]);
    let (mut bufa, mut bufb) = ([[0u8; 4096]; 2], [[0u8; 4096]; 2]);
    let mut sa = bufa.each_mut().map(|b| Socket::new(b));
    let mut sb = bufb.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let ping = a.bind(Proto::Icmp, ICMP_ID).unwrap();
    let client = a.bind(Proto::Udp, CLIENT_PORT).unwrap();
    let server = b.bind(Proto::Udp, ECHO_PORT).unwrap();
    let (mut pinged, mut echoed) = ([false; ITEMS], [false; ITEMS]);
    let mut sent = [None::<u64>; ITEMS];
    let mut received = Vec::new();
    let mut buf = [0u8; 2048];
    while !(pinged.iter().all(|&d| d) && echoed.iter().all(|&d| d)) {
        assert!(
            link.now < limit,
            "not done by {} ms: pings {pinged:?}, datagrams {echoed:?}",
            link.now / MS
        );
        let now = link.now;
        for i in 0..ITEMS {
            if sent[i].is_some_and(|t| now < t + 50 * MS) {
                continue;
            }
            sent[i] = Some(now);
            if !pinged[i] {
                let _ = a.send_to(
                    &mut link.end(0),
                    now,
                    ping,
                    SocketAddrV4::new(IP_B, 0),
                    &echo_request(i as u16),
                );
            }
            if !echoed[i] {
                let to = SocketAddrV4::new(IP_B, ECHO_PORT);
                let _ = a.send_to(&mut link.end(0), now, client, to, &datagram(i));
            }
        }
        link.now += 100_000;
        let now = link.now;
        a.poll(&mut link.end(0), now);
        b.poll(&mut link.end(1), now);
        while let Some((from, n)) = b.recv_from(server, &mut buf) {
            let _ = b.send_to(&mut link.end(1), now, server, from, &buf[..n]);
        }
        while let Some((from, n)) = a.recv_from(ping, &mut buf) {
            let m = &buf[..n];
            assert_eq!(*from.ip(), IP_B);
            assert_eq!(
                (m[0], m[1], &m[4..6]),
                (0, 0, &ICMP_ID.to_be_bytes()[..]),
                "echo reply header"
            );
            let seq = u16::from_be_bytes([m[6], m[7]]) as usize;
            let mut want = echo_request(seq as u16);
            (want[0], want[2], want[3], want[4], want[5]) = (0, m[2], m[3], m[4], m[5]);
            assert_eq!(m, &want[..], "echo reply {seq} intact");
            pinged[seq] = true;
            received.push(m.to_vec());
        }
        while let Some((from, n)) = a.recv_from(client, &mut buf) {
            assert_eq!(from, SocketAddrV4::new(IP_B, ECHO_PORT));
            let i = buf[0] as usize;
            assert_eq!(&buf[..n], &datagram(i)[..], "datagram {i} intact");
            echoed[i] = true;
            received.push(buf[..n].to_vec());
        }
    }
    received
}

#[test]
fn ping_and_udp_echo_over_a_clean_link() {
    let mut link = Link::new(
        1,
        Faults {
            delay: MS,
            ..Faults::default()
        },
        [MAC_A, MAC_B],
    );
    exchange(&mut link, SEC);
}

#[test]
fn ping_and_udp_echo_under_loss_reordering_duplication_and_corruption() {
    for seed in 0..200 {
        let faults = Faults {
            loss: 100,
            duplicate: 50,
            reorder: 100,
            corrupt: 50,
            delay: MS,
        };
        let mut link = Link::new(seed, faults, [MAC_A, MAC_B]);
        exchange(&mut link, 30 * SEC);
    }
}

/// Feeds `frame` to `stack` and returns (frames transmitted, datagrams delivered), checking it was counted once.
fn feed(
    stack: &mut Stack,
    tap: &mut Tap,
    frame: Vec<u8>,
    sockets: &[net::SocketId],
    seen: &mut Vec<Vec<u8>>,
) -> (usize, usize) {
    let before = stack.counters;
    let sent = tap.tx.len();
    tap.rx.push_back(frame);
    stack.poll(tap, 0);
    let mut buf = [0u8; 2048];
    let mut delivered = 0;
    for &s in sockets {
        while let Some((_, n)) = stack.recv_from(s, &mut buf) {
            seen.push(buf[..n].to_vec());
            delivered += 1;
        }
    }
    let replies = tap.tx.len() - sent;
    assert_eq!(stack.counters.rx, before.rx + 1);
    assert_eq!(
        drops(&stack.counters) - drops(&before) + delivered as u64 + replies.min(1) as u64,
        1,
        "counted once"
    );
    (replies, delivered)
}

/// Applies one mutation; returns false if the Internet checksum may miss it (it cannot tell 0x0000 from 0xffff).
fn mutate(rng: &mut Rng, frame: &mut Vec<u8>) -> bool {
    let len = frame.len() as u64;
    match rng.below(6) {
        1 => frame[rng.below(len.min(64)) as usize] = rng.next() as u8,
        2 => frame[rng.below(len) as usize] = [0, 0xff][rng.below(2) as usize],
        3 => frame.truncate(rng.below(len) as usize),
        4 => frame.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
        5 if len >= 2 => {
            let i = rng.below(len.min(48) / 2) as usize * 2;
            let v = [0u16, 1, 0x7fff, 0x8000, 0xffff][rng.below(5) as usize];
            frame[i..i + 2].copy_from_slice(&v.to_be_bytes());
            return false;
        }
        _ => {
            let bit = rng.below(len * 8) as usize;
            frame[bit / 8] ^= 1 << (bit % 8);
        }
    }
    true
}

#[test]
fn mutated_frames_never_panic_and_are_dropped_and_counted() {
    let mut link = Link::new(
        1,
        Faults {
            delay: MS,
            ..Faults::default()
        },
        [MAC_A, MAC_B],
    );
    link.record = Some(Vec::new());
    let mut valid: HashSet<Vec<u8>> = exchange(&mut link, SEC).into_iter().collect();
    let recorded = link.record.take().unwrap();
    let to_b: Vec<_> = recorded
        .iter()
        .filter(|f| f[..6] == MAC_B || f[..6] == [0xff; 6])
        .cloned()
        .collect();
    let to_a: Vec<_> = recorded
        .iter()
        .filter(|f| f[..6] == MAC_A)
        .cloned()
        .collect();
    assert!(to_a.len() >= 2 * ITEMS && to_b.len() >= 2 * ITEMS);
    valid.extend((0..ITEMS).map(datagram));

    let (mut na, mut nb) = ([Neighbor::EMPTY; 4], [Neighbor::EMPTY; 4]);
    let (mut bufa, mut bufb) = ([[0u8; 4096]; 2], [[0u8; 4096]; 2]);
    let mut sa = bufa.each_mut().map(|b| Socket::new(b));
    let mut sb = bufb.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let socks_a = [
        a.bind(Proto::Icmp, ICMP_ID).unwrap(),
        a.bind(Proto::Udp, CLIENT_PORT).unwrap(),
    ];
    let socks_b = [b.bind(Proto::Udp, ECHO_PORT).unwrap()];
    let (mut tap_a, mut tap_b) = (Tap::new(MAC_A), Tap::new(MAC_B));
    let mut rng = Rng::new(42);
    for round in 0..100_000 {
        let to_a_side = rng.below(2) == 0;
        let frames = if to_a_side { &to_a } else { &to_b };
        let mut frame = frames[rng.below(frames.len() as u64) as usize].clone();
        let single = round % 2 == 0;
        let mut detectable = single;
        for _ in 0..if single { 1 } else { 1 + rng.below(8) } {
            if frame.is_empty() {
                break;
            }
            detectable &= mutate(&mut rng, &mut frame);
        }
        let mut seen = Vec::new();
        if to_a_side {
            feed(&mut a, &mut tap_a, frame, &socks_a, &mut seen);
        } else {
            feed(&mut b, &mut tap_b, frame, &socks_b, &mut seen);
        }
        if detectable {
            for d in seen {
                assert!(
                    valid.contains(&d),
                    "round {round}: a single mutation delivered changed data"
                );
            }
        }
    }
    assert!(drops(&a.counters) > 10_000 && drops(&b.counters) > 10_000);
    assert!(a.counters.checksum > 0 && a.counters.malformed > 0);
}

fn arp(op: u16, sha: Mac, spa: Ipv4Addr, tha: Mac, tpa: Ipv4Addr) -> Vec<u8> {
    let dst = if op == 1 { [0xff; 6] } else { tha };
    let mut f = Vec::new();
    f.extend_from_slice(&dst);
    f.extend_from_slice(&sha);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4]);
    f.extend_from_slice(&op.to_be_bytes());
    f.extend_from_slice(&sha);
    f.extend_from_slice(&spa.octets());
    f.extend_from_slice(&tha);
    f.extend_from_slice(&tpa.octets());
    f
}

fn ip(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 0, 0, n)
}

fn mac(n: u8) -> Mac {
    [2, 0, 0, 0, 1, n]
}

/// Sends a UDP datagram to `to` and returns the destination MAC of the frame, or None if an ARP request went out.
fn send(
    stack: &mut Stack,
    tap: &mut Tap,
    now: u64,
    sock: net::SocketId,
    to: Ipv4Addr,
) -> Option<Mac> {
    let n = tap.tx.len();
    let r = stack.send_to(tap, now, sock, SocketAddrV4::new(to, 9), b"hi");
    let frame = &tap.tx[n..];
    assert_eq!(frame.len(), 1);
    match r {
        Ok(()) => Some(frame[0][..6].try_into().unwrap()),
        Err(Error::Unresolved) => {
            assert_eq!(&frame[0][12..14], &[0x08, 0x06]);
            None
        }
        Err(e) => panic!("{e:?}"),
    }
}

fn feed_all(stack: &mut Stack, tap: &mut Tap, now: u64, frames: impl IntoIterator<Item = Vec<u8>>) {
    tap.rx.extend(frames);
    stack.poll(tap, now);
}

#[test]
fn arp_ignores_spoofed_and_unsolicited_traffic() {
    let mut nb = [Neighbor::EMPTY; 4];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);

    assert_eq!(send(&mut a, &mut tap, 0, s, IP_B), None);
    feed_all(&mut a, &mut tap, 0, [arp(2, MAC_B, IP_B, MAC_A, IP_A)]);
    assert_eq!(send(&mut a, &mut tap, 0, s, IP_B), Some(MAC_B));

    let mut forged = arp(2, EVIL, IP_B, MAC_A, IP_A);
    forged[6..12].copy_from_slice(&MAC_B);
    let ignored = a.counters.ignored + a.counters.malformed;
    feed_all(
        &mut a,
        &mut tap,
        0,
        [
            arp(2, EVIL, IP_B, MAC_A, IP_A),
            arp(1, EVIL, IP_B, [0; 6], ip(9)),
            arp(1, EVIL, IP_B, [0; 6], IP_B),
            arp(2, EVIL, IP_B, [0xff; 6], IP_B),
            forged,
        ],
    );
    assert_eq!(a.counters.ignored + a.counters.malformed, ignored + 5);
    assert_eq!(send(&mut a, &mut tap, 0, s, IP_B), Some(MAC_B));

    assert_eq!(send(&mut a, &mut tap, 0, s, ip(3)), None);
    feed_all(
        &mut a,
        &mut tap,
        0,
        [
            arp(2, mac(3), ip(3), MAC_A, IP_A),
            arp(2, EVIL, ip(3), MAC_A, IP_A),
        ],
    );
    assert_eq!(
        send(&mut a, &mut tap, 0, s, ip(3)),
        Some(mac(3)),
        "the first reply wins"
    );

    let n = tap.tx.len();
    feed_all(&mut a, &mut tap, 0, [arp(1, mac(4), ip(4), [0; 6], IP_A)]);
    assert_eq!(
        tap.tx[n..],
        [arp(2, MAC_A, IP_A, mac(4), ip(4))],
        "a request aimed at us is answered"
    );
    assert_eq!(
        send(&mut a, &mut tap, 0, s, ip(4)),
        Some(mac(4)),
        "and learned"
    );
}

#[test]
fn arp_flood_not_aimed_at_us_never_evicts_a_live_neighbour() {
    let mut nb = [Neighbor::EMPTY; 4];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);
    feed_all(
        &mut a,
        &mut tap,
        0,
        (2..6).map(|n| arp(1, mac(n), ip(n), [0; 6], IP_A)),
    );

    let mut rng = Rng::new(7);
    let flood = (0..10_000).map(|i| {
        let sender = (mac(rng.next() as u8), ip(2 + rng.below(250) as u8));
        match i % 3 {
            0 => arp(
                1,
                sender.0,
                sender.1,
                [0; 6],
                ip(100 + rng.below(100) as u8),
            ),
            1 => arp(2, sender.0, sender.1, MAC_A, IP_A),
            _ => arp(2, sender.0, sender.1, mac(9), ip(200)),
        }
    });
    let ignored = a.counters.ignored;
    feed_all(&mut a, &mut tap, SEC, flood.collect::<Vec<_>>());
    assert_eq!(a.counters.ignored, ignored + 10_000);
    for n in 2..6 {
        assert_eq!(send(&mut a, &mut tap, SEC, s, ip(n)), Some(mac(n)));
    }
}

#[test]
fn arp_evicts_the_least_recently_used_neighbour() {
    let mut nb = [Neighbor::EMPTY; 2];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);
    feed_all(
        &mut a,
        &mut tap,
        0,
        [
            arp(1, mac(2), ip(2), [0; 6], IP_A),
            arp(1, mac(3), ip(3), [0; 6], IP_A),
        ],
    );
    assert_eq!(send(&mut a, &mut tap, 1, s, ip(2)), Some(mac(2)));
    feed_all(&mut a, &mut tap, 2, [arp(1, mac(4), ip(4), [0; 6], IP_A)]);
    assert_eq!(send(&mut a, &mut tap, 3, s, ip(2)), Some(mac(2)));
    assert_eq!(send(&mut a, &mut tap, 3, s, ip(4)), Some(mac(4)));
    assert_eq!(send(&mut a, &mut tap, 3, s, ip(3)), None);
}

#[test]
fn arp_retries_then_gives_up_and_poll_reports_the_deadline() {
    let mut nb = [Neighbor::EMPTY; 2];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);
    assert_eq!(a.poll(&mut tap, 0), None);
    assert_eq!(send(&mut a, &mut tap, 0, s, IP_B), None);
    assert_eq!(
        a.send_to(&mut tap, 0, s, SocketAddrV4::new(IP_B, 9), b"x"),
        Err(Error::Unresolved)
    );
    assert_eq!(a.poll(&mut tap, 0), Some(SEC));
    assert_eq!(a.poll(&mut tap, SEC), Some(2 * SEC));
    assert_eq!(a.poll(&mut tap, 2 * SEC), Some(3 * SEC));
    assert_eq!(a.poll(&mut tap, 3 * SEC), None);
    assert_eq!(
        tap.tx.len(),
        3,
        "three requests, then the neighbour is given up"
    );
    assert_eq!(send(&mut a, &mut tap, 3 * SEC, s, IP_B), None);
}

#[test]
fn routes_off_link_through_the_gateway() {
    let mut nb = [Neighbor::EMPTY; 2];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut no_gw = [Neighbor::EMPTY; 1];
    let mut no_gw_bufs = [[0u8; 256]; 1];
    let mut no_gw_socks = no_gw_bufs.each_mut().map(|b| Socket::new(b));
    let mut c = Stack::new(config(IP_A), &mut no_gw, &mut no_gw_socks);
    let cs = c.bind(Proto::Udp, 9).unwrap();
    let far = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53);
    assert_eq!(
        c.send_to(&mut Tap::new(MAC_A), 0, cs, far, b"x"),
        Err(Error::NoRoute)
    );

    let mut a = Stack::new(
        Config {
            gateway: Some(ip(254)),
            ..config(IP_A)
        },
        &mut nb,
        &mut socks,
    );
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);
    assert_eq!(a.send_to(&mut tap, 0, s, far, b"x"), Err(Error::Unresolved));
    assert_eq!(&tap.tx[0][38..42], &ip(254).octets());
    feed_all(
        &mut a,
        &mut tap,
        0,
        [arp(2, mac(254), ip(254), MAC_A, IP_A)],
    );
    assert_eq!(a.send_to(&mut tap, 0, s, far, b"x"), Ok(()));
    assert_eq!(&tap.tx[1][..6], &mac(254));
    assert_eq!(&tap.tx[1][30..34], &[8, 8, 8, 8]);
}

#[test]
fn socket_table_errors_are_named() {
    let mut nb = [Neighbor::EMPTY; 1];
    let mut bufs = [[0u8; 64]; 2];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let u = a.bind(Proto::Udp, 9).unwrap();
    assert_eq!(a.bind(Proto::Udp, 9), Err(Error::InUse));
    assert_eq!(a.bind(Proto::Udp, 0), Err(Error::Invalid));
    let i = a.bind(Proto::Icmp, 9).unwrap();
    assert_eq!(a.bind(Proto::Udp, 10), Err(Error::TableFull));
    a.close(u);
    let mut tap = Tap::new(MAC_A);
    let to = SocketAddrV4::new(IP_B, 9);
    assert_eq!(a.send_to(&mut tap, 0, u, to, b"x"), Err(Error::Closed));
    assert_eq!(
        a.send_to(&mut tap, 0, i, to, b"not an echo request"),
        Err(Error::Invalid)
    );
    assert_eq!(
        a.send_to(&mut tap, 0, i, to, &[8, 0, 0, 0]),
        Err(Error::Invalid)
    );
    let u = a.bind(Proto::Udp, 10).unwrap();
    assert_eq!(
        a.send_to(&mut tap, 0, u, to, &[0; 1473]),
        Err(Error::TooBig)
    );
}

#[test]
fn a_full_socket_buffer_drops_and_counts() {
    let mut link = Link::new(1, Faults::default(), [MAC_A, MAC_B]);
    let (mut na, mut nb) = ([Neighbor::EMPTY; 1], [Neighbor::EMPTY; 1]);
    let (mut bufa, mut bufb) = ([[0u8; 64]; 1], [[0u8; 100]; 1]);
    let mut sa = bufa.each_mut().map(|b| Socket::new(b));
    let mut sb = bufb.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let sa = a.bind(Proto::Udp, 1).unwrap();
    let sb = b.bind(Proto::Udp, 2).unwrap();
    let to = SocketAddrV4::new(IP_B, 2);
    assert_eq!(
        a.send_to(&mut link.end(0), 0, sa, to, &[1; 40]),
        Err(Error::Unresolved)
    );
    b.poll(&mut link.end(1), 0);
    a.poll(&mut link.end(0), 0);
    for i in 0..3 {
        a.send_to(&mut link.end(0), 0, sa, to, &[i; 40]).unwrap();
    }
    b.poll(&mut link.end(1), 0);
    assert_eq!(b.counters.socket_full, 1);
    let mut buf = [0u8; 16];
    assert_eq!(
        b.recv_from(sb, &mut buf),
        Some((SocketAddrV4::new(IP_A, 1), 16)),
        "truncated"
    );
    assert_eq!(buf, [0; 16]);
    assert_eq!(b.recv_from(sb, &mut buf).map(|r| r.1), Some(16));
    assert_eq!(buf, [1; 16]);
    assert_eq!(b.recv_from(sb, &mut buf), None);
}

#[test]
fn ipv4_fragments_are_dropped_and_counted() {
    let (mut na, mut nb) = ([Neighbor::EMPTY; 1], [Neighbor::EMPTY; 1]);
    let (mut bufa, mut bufb) = ([[0u8; 256]; 1], [[0u8; 256]; 1]);
    let mut sa = bufa.each_mut().map(|b| Socket::new(b));
    let mut sb = bufb.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut na, &mut sa);
    let mut b = Stack::new(config(IP_B), &mut nb, &mut sb);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let server = b.bind(Proto::Udp, 9).unwrap();
    let (mut tap_a, mut tap_b) = (Tap::new(MAC_A), Tap::new(MAC_B));
    feed_all(&mut a, &mut tap_a, 0, [arp(1, MAC_B, IP_B, [0; 6], IP_A)]);
    assert_eq!(send(&mut a, &mut tap_a, 0, s, IP_B), Some(MAC_B));
    let whole = tap_a.tx.pop().unwrap();
    let fragments = [(0x20, 0), (0, 1), (0x21, 0x80)].map(|(hi, lo)| {
        let mut f = whole.clone();
        (f[20], f[21]) = (f[20] | hi, lo);
        f[24..26].fill(0);
        let sum = f[14..34]
            .chunks(2)
            .map(|w| u16::from_be_bytes([w[0], w[1]]) as u32)
            .sum::<u32>();
        let sum = (sum & 0xffff) + (sum >> 16);
        let c = !((sum & 0xffff) + (sum >> 16)) as u16;
        f[24..26].copy_from_slice(&c.to_be_bytes());
        f
    });
    feed_all(&mut b, &mut tap_b, 0, fragments);
    assert_eq!(b.counters.fragments, 3);
    feed_all(&mut b, &mut tap_b, 0, [whole]);
    assert_eq!(
        b.recv_from(server, &mut [0; 8]),
        Some((SocketAddrV4::new(IP_A, 9), 2))
    );
}

#[test]
fn arp_never_learns_unasked_or_invalid_senders() {
    let mut nb = [Neighbor::EMPTY; 8];
    let mut bufs = [[0u8; 256]; 1];
    let mut socks = bufs.each_mut().map(|b| Socket::new(b));
    let mut a = Stack::new(config(IP_A), &mut nb, &mut socks);
    let s = a.bind(Proto::Udp, 9).unwrap();
    let mut tap = Tap::new(MAC_A);
    let dropped = a.counters.ignored + a.counters.malformed;
    feed_all(
        &mut a,
        &mut tap,
        0,
        [
            arp(2, mac(5), ip(5), MAC_A, IP_A),
            arp(1, [0xff; 6], ip(6), [0; 6], IP_A),
            arp(1, [0; 6], ip(7), [0; 6], IP_A),
            arp(1, [1, 0, 0x5e, 0, 0, 1], ip(8), [0; 6], IP_A),
            arp(1, mac(9), IP_A, [0; 6], IP_A),
            arp(1, mac(10), ip(255), [0; 6], IP_A),
            arp(1, mac(11), Ipv4Addr::new(10, 0, 1, 11), [0; 6], IP_A),
        ],
    );
    assert_eq!(a.counters.ignored + a.counters.malformed, dropped + 7);
    assert!(tap.tx.is_empty(), "no reply to an invalid sender");
    for n in 5..=8 {
        assert_eq!(
            send(&mut a, &mut tap, 0, s, ip(n)),
            None,
            "10.0.0.{n} not learned"
        );
    }
}
