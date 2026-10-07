//! `test=sockets`'s init and its children. As init (no arguments): runs the C `tcpecho` server and client on musl's
//! BSD sockets, then `nettest serve` (one process serving 8 connections at once through `io_wait`) with
//! `nettest connect` (8 clients, the same way), then children that must fail: one without the NetStack handle, one
//! with a listen-only duplicate, one whose budget holds 3 sockets. `nettest bench` (`test=bench-sockets`) times
//! loopback TCP against `nettest benchserve`.
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// A child's NetStack: after its console.
const CHILD_NET: u64 = 1;
const CONNECTIONS: usize = 8;
const ECHO_PORT: u16 = 7;
/// The kernel's per-socket charge (`SOCKET_FRAMES`).
const SOCKET_FRAMES: usize = 8;
/// `nettest`'s own frames, measured: 14 with a `map`'s page and table, 12 without (the budget child's 3 sockets fit either way).
const OWN_BUDGET: usize = 14;
/// A C program on musl: its image, the 128 KiB stack `__mog_start` maps, heap, and one socket.
const C_BUDGET: usize = 128;
const ACCEPT: u64 = u64::MAX;
/// Tags: connection `i`'s receive is `i`, its send `SEND + i`, its connect `CONNECT_TAG + i`.
const SEND: u64 = 100;
const CONNECT_TAG: u64 = 200;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: x0 and x2 as the kernel started this process.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    match arg(args, 1) {
        b"serve" => serve(),
        b"bench" => bench(),
        b"benchserve" => bench_serve(),
        b"connect" => connect_all(),
        b"nonet" => report(b"no handle", socket(CHILD_NET)),
        b"listenonly" => {
            let sock = socket(CHILD_NET);
            report(
                b"listen-only connect",
                connect(sock as u64, [127, 0, 0, 1], 1, 0),
            )
        }
        b"hold" => 0,
        b"readaccept" => {
            // Handle 1 is a read-only listener: what it accepts must not be writable.
            let conn = match accept(1, 0) {
                0 => wait_for(0),
                error => error,
            };
            report(b"read-only accept send", send(conn as u64, b"x"))
        }
        b"budget" => {
            let mut n = 0;
            loop {
                match socket(CHILD_NET) {
                    s if s >= 0 => n += 1,
                    ENOBUFS => break,
                    error => return status(error),
                }
            }
            write(CONSOLE, b"nettest: ENOBUFS after ");
            write_u64(CONSOLE, n);
            write(CONSOLE, b" sockets\n");
            0
        }
        _ => init(),
    }
}

/// Prints `nettest: <what>: <errno name>` for an expected failure; exits on anything else.
fn report(what: &[u8], result: i64) -> u64 {
    let name: &[u8] = match result {
        EBADF => b"EBADF",
        EACCES => b"EACCES",
        _ => return 1,
    };
    write(CONSOLE, b"nettest: ");
    write(CONSOLE, what);
    write(CONSOLE, b": ");
    write(CONSOLE, name);
    write(CONSOLE, b"\n");
    0
}

fn init() -> u64 {
    let c = open(DIR, b"tcpecho", 0) as u64;
    let posix = |net: u64| {
        let console = || dup(CONSOLE, WRITE | READ | DUPLICATE | TRANSFER) as u64;
        // Handle 3, the root, is a transfer-only placeholder; 4 is the archive, which `tcpecho p` spawns from.
        let placeholder = dup(CONSOLE, TRANSFER) as u64;
        let archive = dup(DIR, READ | EXEC | DUPLICATE | TRANSFER) as u64;
        [console(), console(), console(), placeholder, archive, net]
    };
    let server = spawn_at(c, &posix(net()), C_BUDGET, u64::MAX, b"tcpecho\0s\0");
    let client = spawn_at(c, &posix(net()), C_BUDGET, u64::MAX, b"tcpecho\0c\0");
    if !reaped(server) || !reaped(client) {
        return 1;
    }
    // A C program on musl holding a NetStack it could pass on (duplicate right) spawns a child, which must not get it.
    let parent = spawn_at(
        c,
        &posix(dup(net_handle(), CONNECT | LISTEN | DUPLICATE | TRANSFER) as u64),
        2 * C_BUDGET + 64,
        u64::MAX,
        b"tcpecho\0p\0",
    );
    if !reaped(parent) {
        return 4;
    }
    let me = open(DIR, b"nettest", 0) as u64;
    let child = |args: &[u8], handles: &[u64], sockets: usize| {
        let budget = OWN_BUDGET + sockets * SOCKET_FRAMES;
        spawn_at(me, handles, budget, u64::MAX, args)
    };
    // The listener and its backlog of 8, and the 8 connections.
    let server = child(
        b"nettest\0serve\0",
        &[console(), net()],
        1 + 2 * CONNECTIONS,
    );
    let client = child(b"nettest\0connect\0", &[console(), net()], CONNECTIONS);
    if !reaped(server) || !reaped(client) {
        return 2;
    }
    let listen_only = dup(net_handle(), LISTEN | TRANSFER) as u64;
    for (args, handles, sockets) in [
        (&b"nettest\0nonet\0"[..], &[console()][..], 0),
        (b"nettest\0listenonly\0", &[console(), listen_only], 1),
        (b"nettest\0budget\0", &[console(), net()], 3),
    ] {
        let process = child(args, handles, sockets);
        if !reaped(process) {
            return 3;
        }
    }
    if !transfer() {
        return 9;
    }
    read_only_accept()
}

/// A socket moved to a child is charged to the child before anything else: a budget without room for it fails the
/// spawn with `ENOBUFS`.
fn transfer() -> bool {
    let sock = socket(net_handle());
    let me = open(DIR, b"nettest", 0) as u64;
    let short = spawn_at(
        me,
        &[console(), sock as u64],
        SOCKET_FRAMES - 1,
        u64::MAX,
        b"nettest\0hold\0",
    );
    let fits = OWN_BUDGET + SOCKET_FRAMES;
    let child = spawn_at(
        me,
        &[console(), sock as u64],
        fits,
        u64::MAX,
        b"nettest\0hold\0",
    );
    if sock < 0 || short != ENOBUFS || !reaped(child) {
        return false;
    }
    write(
        CONSOLE,
        b"nettest: a moved socket is charged to its new holder\n",
    );
    true
}

/// A child accepts through a read-only duplicate of a listener; the connection's handle must not write.
fn read_only_accept() -> u64 {
    let net = net_handle();
    let listener = socket(net) as u64;
    if bind(listener, 12) != 0 || listen(listener, 1) != 0 {
        return 5;
    }
    let read_only = dup(listener, READ | TRANSFER) as u64;
    let child = open(DIR, b"nettest", 0) as u64;
    // The listener with its backlog of 1, then the connection it accepts.
    let budget = OWN_BUDGET + 3 * SOCKET_FRAMES;
    let process = spawn_at(
        child,
        &[console(), read_only],
        budget,
        u64::MAX,
        b"nettest\0readaccept\0",
    );
    let client = socket(net) as u64;
    if process < 0 || connect(client, [127, 0, 0, 1], 12, 0) != 0 || wait_for(0) != 0 {
        return 6;
    }
    if !reaped(process) {
        return 7;
    }
    close(client);
    close(listener);
    0
}

/// Waits for `process` to exit 0, then closes its handle (init's table has room for few).
fn reaped(process: i64) -> bool {
    process >= 0 && wait(process as u64) == 0 && close(process as u64) == 0
}

fn console() -> u64 {
    dup(CONSOLE, WRITE | TRANSFER) as u64
}

fn net() -> u64 {
    dup(net_handle(), CONNECT | LISTEN | TRANSFER) as u64
}

/// `bench`'s rounds: 64-byte round trips, connect + close pairs, and 4 KiB sends streamed (16 MiB).
const ROUND_TRIPS: u64 = 10_000;
const CONNECTS: u64 = 1000;
const CHUNKS: u64 = 4096;
const BENCH_PORT: u16 = 9;

/// Times, against `benchserve`: a 64-byte send + receive round trip, a connect + close, and a 4 KiB send of a
/// stream; prints each as a `bench` line.
fn bench() -> u64 {
    let me = open(DIR, b"nettest", 0) as u64;
    let budget = OWN_BUDGET + 3 * SOCKET_FRAMES;
    let server = spawn_at(
        me,
        &[console(), net()],
        budget,
        u64::MAX,
        b"nettest\0benchserve\0",
    );
    let buf = map(4096).unwrap_or_else(|| exit(7));
    let net = net_handle();
    let sock = dial(net);
    let start = now_ns();
    for _ in 0..ROUND_TRIPS {
        if send(sock, &buf[..64]) != 0 || !fill(sock, &mut buf[..64]) {
            return 1;
        }
    }
    report_ns(b"tcp-rtt", start, ROUND_TRIPS);
    close(sock);
    let start = now_ns();
    for _ in 0..CONNECTS {
        close(dial(net));
    }
    report_ns(b"tcp-connect", start, CONNECTS);
    let sock = dial(net);
    let start = now_ns();
    for _ in 0..CHUNKS {
        if send(sock, buf) != 0 {
            return 2;
        }
    }
    shutdown(sock);
    // The server's one byte says it read the whole stream.
    if receive(sock, &mut buf[..1]) != 1 {
        return 3;
    }
    report_ns(b"tcp-stream-4k", start, CHUNKS);
    close(sock);
    if server < 0 || wait(server as u64) != 0 {
        return 4;
    }
    0
}

/// A connection to `BENCH_PORT`, retrying while refused (the server may not listen yet).
fn dial(net: u64) -> u64 {
    loop {
        let sock = socket(net) as u64;
        match connect(sock, [127, 0, 0, 1], BENCH_PORT, 0) {
            0 => match wait_for(0) {
                0 => return sock,
                ECONNREFUSED => close(sock),
                _ => exit(5),
            },
            _ => exit(6),
        };
    }
}

/// Receives exactly `buf.len()` bytes; false at the end of the stream or on an error.
fn fill(sock: u64, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        match receive(sock, &mut buf[got..]) {
            n if n > 0 => got += n as usize,
            _ => return false,
        }
    }
    true
}

fn report_ns(name: &[u8], start: u64, n: u64) {
    write(CONSOLE, b"bench ");
    write(CONSOLE, name);
    write(CONSOLE, b": ");
    write_u64(CONSOLE, (now_ns() - start) / n);
    write(CONSOLE, b" ns\n");
}

/// `bench`'s other end: echoes the first connection, accepts and closes `CONNECTS`, then reads a stream to its end
/// and answers one byte.
fn bench_serve() -> u64 {
    let listener = socket(CHILD_NET) as u64;
    if bind(listener, BENCH_PORT) != 0 || listen(listener, 1) != 0 {
        return 1;
    }
    let next = || match accept(listener, 0) {
        0 => wait_for(0),
        error => error,
    };
    let buf = map(4096).unwrap_or_else(|| exit(4));
    let sock = next() as u64;
    loop {
        match receive(sock, buf) {
            0 => break,
            n if n > 0 && send(sock, &buf[..n as usize]) == 0 => {}
            _ => return 2,
        }
    }
    close(sock);
    for _ in 0..CONNECTS {
        close(next() as u64);
    }
    let sock = next() as u64;
    while receive(sock, buf) > 0 {}
    if send(sock, b"!") != 0 {
        return 3;
    }
    close(sock);
    0
}

/// Receive buffers, one per connection, in memory nothing else references.
fn buffers() -> &'static mut [u8] {
    map(CONNECTIONS * 64).unwrap_or_else(|| exit(10))
}

/// Accepts `CONNECTIONS` connections on `ECHO_PORT`, all served at once: each received chunk is sent back, and a
/// connection closes at its end of stream.
fn serve() -> u64 {
    let listener = socket(CHILD_NET) as u64;
    if bind(listener, ECHO_PORT) != 0
        || listen(listener, CONNECTIONS as u64) != 0
        || accept(listener, ACCEPT) != 0
    {
        return 1;
    }
    let buf = buffers().as_mut_ptr() as u64;
    let (mut conns, mut accepted, mut closed) = ([0; CONNECTIONS], 0, 0);
    while closed < CONNECTIONS {
        let (result, tag) = io_wait();
        if result < 0 {
            return 2;
        }
        let i = (tag % SEND) as usize;
        // Connection `i`'s 64 bytes of `buf` are only ever in its one op in flight.
        let at = |i: usize| buf + 64 * i as u64;
        // SAFETY: as above.
        let receive = |sock, i: usize| unsafe { io_submit(sock, OP_RECEIVE, at(i), 64, i as u64) };
        let submitted = match tag {
            ACCEPT => {
                conns[accepted] = result as u64;
                accepted += 1;
                let more = if accepted < CONNECTIONS {
                    accept(listener, ACCEPT)
                } else {
                    0
                };
                more | receive(result as u64, accepted - 1)
            }
            _ if tag >= SEND => receive(conns[i], i),
            _ if result == 0 => {
                closed += 1;
                close(conns[i])
            }
            // SAFETY: as above.
            _ => unsafe { io_submit(conns[i], OP_SEND, at(i), result as usize, SEND + i as u64) },
        };
        if submitted != 0 {
            return 3;
        }
    }
    write(CONSOLE, b"nettest: served 8\n");
    0
}

/// Opens `CONNECTIONS` connections to `ECHO_PORT` at once (retrying refused ones: the server may not listen yet),
/// sends each `echo <i>` and checks the reply, all through `io_wait`; then ends each stream.
fn connect_all() -> u64 {
    let buf = buffers();
    let mut socks = [0; CONNECTIONS];
    for (i, sock) in socks.iter_mut().enumerate() {
        *sock = socket(CHILD_NET) as u64;
        if connect(*sock, [127, 0, 0, 1], ECHO_PORT, CONNECT_TAG + i as u64) != 0 {
            return 1;
        }
    }
    let message = |i: usize| [b'e', b'c', b'h', b'o', b' ', b'0' + i as u8];
    let mut echoes = 0;
    while echoes < CONNECTIONS {
        let (result, tag) = io_wait();
        let i = (tag % SEND) as usize;
        let at = buf.as_mut_ptr() as u64 + 64 * i as u64;
        let submitted = match tag {
            _ if tag >= CONNECT_TAG && result == ECONNREFUSED => {
                close(socks[i]);
                socks[i] = socket(CHILD_NET) as u64;
                connect(socks[i], [127, 0, 0, 1], ECHO_PORT, tag)
            }
            _ if result < 0 => return 2,
            _ if tag >= CONNECT_TAG => {
                buf[64 * i..64 * i + 6].copy_from_slice(&message(i));
                // SAFETY: connection `i`'s 64 bytes of `buf` are only touched between its ops.
                unsafe { io_submit(socks[i], OP_SEND, at, 6, SEND + i as u64) }
            }
            // SAFETY: as above.
            _ if tag >= SEND => unsafe { io_submit(socks[i], OP_RECEIVE, at, 64, i as u64) },
            _ if buf[64 * i..64 * i + result as usize] == message(i) => {
                echoes += 1;
                shutdown(socks[i]) | close(socks[i])
            }
            _ => return 3,
        };
        if submitted != 0 {
            return 4;
        }
    }
    write(CONSOLE, b"nettest: 8 echoes\n");
    0
}
