//! `test=httpd`'s init and the HTTP echo server. As init (`httpd <requests> [<fetch target> [<times>]]`): runs
//! `fetch` on the target with a connect-only NetStack, then `httpd serve <requests>` with a listen-only one. The
//! server answers each request on port 80 with `200 OK`, `text/plain`, and the request itself (request line,
//! headers, body) as the body, one connection at a time, until it has served `<requests>` (0: forever).
#![no_std]
#![no_main]

use user::*;

/// init's boot-archive directory handle.
const DIR: u64 = 2;
/// The server's NetStack: after its console.
const NET: u64 = 1;
const PORT: u16 = 80;
/// Largest request head (request line and headers), and body: the body streams through, so its bound is a choice.
const HEAD: usize = 8192;
const BODY: u64 = 1 << 30;
/// The server's and `fetch`'s own frames (measured, as `nettest`'s) and their sockets': the server's listener with a
/// backlog of 1, and a connection.
const SERVER_BUDGET: usize = 16 + 3 * 8;
const FETCH_BUDGET: usize = 16 + 8;
const OK: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: ";

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: x0 and x2 as the kernel started this process.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    if arg(args, 1) == b"serve" {
        return serve(number(arg(args, 2)));
    }
    let net = net_handle();
    let console = || dup(CONSOLE, WRITE | TRANSFER) as u64;
    if args.len() > 2 {
        let fetch = open(DIR, b"fetch", 0) as u64;
        let mut fetch_args = [0; 128];
        let n = join(
            &mut fetch_args,
            &[b"fetch", arg(args, 2), arg(args, 3)][..args.len() - 1],
        );
        let handles = [console(), dup(net, CONNECT | TRANSFER) as u64];
        let process = spawn_at(fetch, &handles, FETCH_BUDGET, u64::MAX, &fetch_args[..n]);
        if process < 0 || wait(process as u64) != 0 {
            return 1;
        }
    }
    let me = open(DIR, b"httpd", 0) as u64;
    let mut serve_args = [0; 64];
    let n = join(&mut serve_args, &[b"httpd", b"serve", arg(args, 1)]);
    let handles = [console(), dup(net, LISTEN | TRANSFER) as u64];
    let process = spawn_at(me, &handles, SERVER_BUDGET, u64::MAX, &serve_args[..n]);
    if process < 0 || wait(process as u64) != 0 {
        return 2;
    }
    0
}

/// Writes `words` into `out`, each ending in a NUL; returns the length.
fn join(out: &mut [u8], words: &[&[u8]]) -> usize {
    let mut n = 0;
    for word in words {
        out[n..n + word.len()].copy_from_slice(word);
        out[n + word.len()] = 0;
        n += word.len() + 1;
    }
    n
}

/// Serves `requests` requests (0: forever), one connection at a time.
fn serve(requests: u64) -> u64 {
    let listener = socket(NET) as u64;
    if bind(listener, PORT) != 0 || listen(listener, 1) != 0 {
        return 1;
    }
    let buf = map(HEAD).unwrap_or_else(|| exit(2));
    let mut served = 0;
    while requests == 0 || served < requests {
        let conn = match accept(listener, 0) {
            0 => wait_for(0),
            error => error,
        };
        if conn < 0 {
            return 3;
        }
        echo(conn as u64, buf);
        close(conn as u64);
        served += 1;
    }
    0
}

/// Answers one request on `conn` with itself; gives up on a head over `HEAD` bytes or a broken connection.
fn echo(conn: u64, buf: &mut [u8]) {
    let mut got = 0;
    let head = loop {
        if let Some(end) = buf[..got].windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        if got == buf.len() {
            return refuse(conn, b"431 Request Header Fields Too Large");
        }
        match receive(conn, &mut buf[got..]) {
            n if n > 0 => got += n as usize,
            _ => return,
        }
    };
    let body = match body_length(&buf[..head], BODY) {
        Ok(body) => body,
        Err(400) => return refuse(conn, b"400 Bad Request"),
        Err(_) => return refuse(conn, b"413 Content Too Large"),
    };
    // At most `HEAD + BODY`: no overflow.
    let total = head as u64 + body;
    let mut digits = [0; 20];
    if send(conn, OK) != 0
        || send(conn, decimal(&mut digits, total)) != 0
        || send(conn, b"\r\n\r\n") != 0
    {
        return;
    }
    let mut left = total;
    let mut chunk = got.min(total as usize);
    while left > 0 {
        if send(conn, &buf[..chunk]) != 0 {
            return;
        }
        left -= chunk as u64;
        if left == 0 {
            break;
        }
        let want = buf.len().min(left as usize);
        chunk = match receive(conn, &mut buf[..want]) {
            n if n > 0 => n as usize,
            _ => return,
        };
    }
}

/// Answers `status` with no body.
fn refuse(conn: u64, status: &[u8]) {
    for part in [
        b"HTTP/1.1 ",
        status,
        b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ] {
        if send(conn, part) != 0 {
            return;
        }
    }
}

/// A decimal number; 0 if `s` is not one.
fn number(s: &[u8]) -> u64 {
    s.iter()
        .try_fold(0u64, |n, &d| {
            d.is_ascii_digit()
                .then(|| n.checked_mul(10)?.checked_add((d - b'0').into()))?
        })
        .unwrap_or(0)
}

/// `n` in decimal, at the end of `digits`.
fn decimal(digits: &mut [u8; 20], mut n: u64) -> &[u8] {
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &digits[i..];
        }
    }
}
