//! Networking: the stack the board's net task runs, configured by the `net=<ip>/<prefix>[,gw=<ip>]` bootarg.

use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{Ipv4Addr, SocketAddrV4};

use net::{Config, Error, Neighbor, Proto, Socket, SocketId, Stack};

use crate::Board;

const NEIGHBORS: usize = 8;
const SOCKETS: usize = 4;
/// Each UDP or ICMP socket's receive buffer.
const SOCKET_BUFFER: usize = 4096;

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

/// A stack at `config` whose tables live on the kernel heap until power-off; `None` if the heap is short.
pub fn stack(config: Config) -> Option<Stack<'static>> {
    let neighbors = leak(NEIGHBORS, || Neighbor::EMPTY)?;
    let mut sockets = Vec::new();
    sockets.try_reserve_exact(SOCKETS).ok()?;
    for _ in 0..SOCKETS {
        sockets.push(Socket::new(leak(SOCKET_BUFFER, || 0)?));
    }
    Some(Stack::new(config, neighbors, sockets.leak()))
}

fn leak<T>(n: usize, value: impl FnMut() -> T) -> Option<&'static mut [T]> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.extend(core::iter::repeat_with(value).take(n));
    Some(v.leak())
}

/// `test=net`: pings the gateway, sends `mog` to the UDP echo at the gateway's port `port` and prints both replies,
/// then the frame counters.
pub fn net_test<B: Board>(board: &mut B, gateway: Ipv4Addr, port: u16) {
    let ping = board
        .with_net(|stack, _, _| stack.bind(Proto::Icmp, 1))
        .expect("bind");
    let request = [8, 0, 0, 0, 0, 0, 0, 1, b'm', b'o', b'g'];
    let (from, _) = round_trip(board, ping, SocketAddrV4::new(gateway, 0), &request);
    let _ = writeln!(board.console(), "ping: reply from {}", from.ip());
    let udp = board
        .with_net(|stack, _, _| stack.bind(Proto::Udp, 7))
        .expect("bind");
    let (from, reply) = round_trip(board, udp, SocketAddrV4::new(gateway, port), b"mog");
    let reply = core::str::from_utf8(&reply).unwrap_or("?");
    let _ = writeln!(board.console(), "udp: echo {reply} from {from}");
    let c = board.with_net(|stack, _, _| stack.counters);
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
    let udp = board
        .with_net(|stack, _, _| stack.bind(Proto::Udp, 7))
        .expect("bind");
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
    while board
        .with_net(|stack, _, _| stack.recv_from(udp, &mut []))
        .is_some()
    {}
    let start = board.uptime_us();
    let (mut sent, mut received) = (0, 0);
    while received < BENCH_DATAGRAMS {
        if sent < BENCH_DATAGRAMS && sent - received < BENCH_WINDOW {
            send(board, udp, to, &data);
            sent += 1;
        } else if board
            .with_net(|stack, _, _| stack.recv_from(udp, &mut []))
            .is_some()
        {
            received += 1;
        } else {
            board.idle();
            board.run_others();
        }
    }
    bench(board, "udp-stream", start, BENCH_DATAGRAMS);
}

fn bench<B: Board>(board: &mut B, name: &str, start: u64, n: u64) {
    let ns = (board.uptime_us() - start) * 1000 / n;
    let _ = writeln!(board.console(), "bench {name}: {ns} ns");
}

fn send<B: Board>(board: &mut B, socket: SocketId, to: SocketAddrV4, data: &[u8]) {
    while let Err(Error::Unresolved | Error::Busy) =
        board.with_net(|stack, nic, now| stack.send_to(nic, now, socket, to, data))
    {
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
        if let Some((from, n)) = board.with_net(|stack, _, _| stack.recv_from(socket, &mut buf)) {
            return (from, buf[..n].to_vec());
        }
        board.idle();
        board.run_others();
    }
}
