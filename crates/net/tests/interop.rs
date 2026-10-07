//! Our TCP against smoltcp's over the simulated link, each side opening, under loss.
mod sim;

use std::net::{Ipv4Addr, SocketAddrV4};

use net::{Config, Error, HalfOpen, Mac, Neighbor, Nic, Stack, State, Tcp, TcpSocket, TimeWait};
use sim::{Faults, Link};
use smoltcp::iface::{Config as SmolConfig, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

const MAC_A: Mac = [2, 0, 0, 0, 0, 1];
const MAC_B: Mac = [2, 0, 0, 0, 0, 2];
const IP_A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const PORT: u16 = 80;
const RING: usize = 64 << 10;

/// smoltcp's side of the link.
struct Smol<'l>(&'l mut Link);

struct Rx(Vec<u8>);

struct Tx<'a>(&'a mut Link);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut r = None;
        self.0.end(1).transmit(len, |b| r = Some(f(b)));
        r.unwrap()
    }
}

impl Device for Smol<'_> {
    type RxToken<'a>
        = Rx
    where
        Self: 'a;
    type TxToken<'a>
        = Tx<'a>
    where
        Self: 'a;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let mut frame = None;
        self.0.end(1).receive(|f| frame = Some(f.to_vec()));
        Some((Rx(frame?), Tx(self.0)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        (c.medium, c.max_transmission_unit) = (Medium::Ethernet, 1514);
        c
    }
}

fn instant(ns: u64) -> Instant {
    Instant::from_micros((ns / 1000) as i64)
}

fn pattern(k: usize) -> u8 {
    (k as u32).wrapping_mul(2_654_435_761).to_be_bytes()[0]
}

/// Bytes sent and received on one side, checked against the pattern.
#[derive(Default)]
struct Flow {
    sent: usize,
    received: usize,
    eof: bool,
}

impl Flow {
    fn check(&mut self, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            assert_eq!(
                b,
                pattern(self.received + i),
                "byte {} intact",
                self.received + i
            );
        }
        self.received += data.len();
    }
}

/// Our stack (A) and smoltcp (B) send `bytes` each way at once, then close; `we_connect` picks who opens.
fn interop(seed: u64, loss: u64, bytes: usize, we_connect: bool) {
    let faults = Faults {
        loss,
        duplicate: 10,
        reorder: 20,
        corrupt: 5,
        delay: MS,
    };
    let mut link = Link::new(seed, faults, [MAC_A, MAC_B]);
    let mut neighbors = [Neighbor::EMPTY; 4];
    let (mut rx, mut tx) = (vec![vec![0u8; RING]; 2], vec![vec![0u8; RING]; 2]);
    let mut slots: Vec<TcpSocket> = rx
        .iter_mut()
        .zip(tx.iter_mut())
        .map(|(r, t)| TcpSocket::new(r, t))
        .collect();
    let (mut half_open, mut time_wait) = ([HalfOpen::EMPTY; 4], [TimeWait::EMPTY; 4]);
    let config = Config {
        ip: IP_A,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: None,
    };
    let mut a = Stack::new(config, &mut neighbors, &mut []).with_tcp(Tcp::new(
        [seed, 1],
        &mut slots,
        &mut half_open,
        &mut time_wait,
    ));

    let mut smol_config = SmolConfig::new(EthernetAddress(MAC_B).into());
    smol_config.random_seed = seed;
    let mut iface = Interface::new(smol_config, &mut Smol(&mut link), instant(0));
    iface.update_ip_addrs(|ips| {
        ips.push(IpCidr::new(IpAddress::v4(10, 0, 0, 2), 24))
            .unwrap()
    });
    let mut sockets = SocketSet::new(vec![]);
    let socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; RING]),
        tcp::SocketBuffer::new(vec![0; RING]),
    );
    let hb = sockets.add(socket);

    let (listener, mut ca) = if we_connect {
        sockets.get_mut::<tcp::Socket>(hb).listen(PORT).unwrap();
        (None, a.connect(0, 0, SocketAddrV4::new(IP_B, PORT)).ok())
    } else {
        let to = (IpAddress::v4(10, 0, 0, 1), PORT);
        sockets
            .get_mut::<tcp::Socket>(hb)
            .connect(iface.context(), to, 49152)
            .unwrap();
        (Some(a.listen(PORT).unwrap()), None)
    };
    let (mut fa, mut fb) = (Flow::default(), Flow::default());
    let mut buf = vec![0u8; 8192];
    let src: Vec<u8> = (0..bytes).map(pattern).collect();
    loop {
        let now = link.now;
        assert!(
            now < 3600 * SEC,
            "seed {seed}: not done in an hour of virtual time"
        );
        let da = a.poll(&mut link.end(0), now);
        iface.poll(instant(now), &mut Smol(&mut link), &mut sockets);
        let mut progress = false;

        ca = ca.or_else(|| listener.and_then(|l| a.accept(l)));
        if let Some(ca) = ca {
            if fa.sent < bytes
                && let Ok(n) = a.send(ca, &src[fa.sent..])
            {
                (fa.sent, progress) = (fa.sent + n, true);
            }
            // smoltcp 0.12 drops its retransmission timer when a FIN moves it to CLOSE-WAIT or CLOSING, so a FIN
            // goes to it only once its data has all arrived.
            if fa.sent == bytes
                && fa.received == bytes
                && a.tcp_info(ca).unwrap().state == State::Established
            {
                a.shutdown(ca);
                progress = true;
            }
            loop {
                match a.recv(ca, &mut buf) {
                    Ok(0) => {
                        progress |= !fa.eof;
                        fa.eof = true;
                        break;
                    }
                    Ok(n) => (fa.check(&buf[..n]), progress = true).1,
                    Err(Error::WouldBlock) => break,
                    Err(e) => panic!(
                        "seed {seed} loss {loss}: our recv: {e:?} {:?} {:?} smol {:?}",
                        a.tcp_info(ca),
                        a.counters,
                        sockets.get::<tcp::Socket>(hb).state()
                    ),
                };
            }
        }
        let s = sockets.get_mut::<tcp::Socket>(hb);
        if s.can_send() && fb.sent < bytes {
            let n = s.send_slice(&src[fb.sent..]).unwrap();
            (fb.sent, progress) = (fb.sent + n, progress || n > 0);
        }
        while s.can_recv() {
            let n = s.recv_slice(&mut buf).unwrap();
            fb.check(&buf[..n]);
            progress = true;
        }
        if !s.may_recv() && !fb.eof && fb.received == bytes {
            (fb.eof, progress) = (true, true);
        }
        // For the same reason smoltcp closes only once its data is acknowledged and ours has ended.
        if fb.eof && fb.sent == bytes && s.send_queue() == 0 && s.state() == tcp::State::CloseWait {
            s.close();
            progress = true;
        }
        let ours_closed = ca.is_some_and(|c| a.tcp_info(c).unwrap().state == State::Closed);
        let smol_closed = matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait);
        if fa.eof && fb.eof && ours_closed && smol_closed {
            break;
        }
        if progress {
            continue;
        }
        let smol_at = iface
            .poll_at(instant(now), &sockets)
            .map(|t| t.total_micros() as u64 * 1000);
        let next = [da, link.next(), smol_at]
            .into_iter()
            .flatten()
            .min()
            .expect("stalled");
        link.now = next.max(now + 1000);
    }
    assert_eq!((fa.received, fb.received), (bytes, bytes), "seed {seed}");
    assert_eq!(a.tcp_info(ca.unwrap()).unwrap().error, None);
}

#[test]
fn our_client_and_smoltcp_server_exchange_data_under_loss() {
    for (seed, loss) in (0..20).flat_map(|s| [(s, 0), (s, 10), (s, 50)]) {
        interop(seed, loss, 256 << 10, true);
    }
}

#[test]
fn smoltcp_client_and_our_server_exchange_data_under_loss() {
    for (seed, loss) in (0..20).flat_map(|s| [(s, 0), (s, 10), (s, 50)]) {
        interop(seed, loss, 256 << 10, false);
    }
}
