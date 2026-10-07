//! `fetch <ip>:<port>[/<path>] [<times>]`: GETs the page over HTTP/1.0 with handle 1, a NetStack, and prints its
//! body. With `<times>`, fetches it that many times without printing and reports `bench http-get: <ns> ns`, the mean
//! from connect to the end of the body.
#![no_std]
#![no_main]

use user::*;

const NET: u64 = 1;

#[unsafe(no_mangle)]
extern "C" fn _start(argc: usize, _: usize, len: usize) -> ! {
    // SAFETY: x0 and x2 as the kernel started this process.
    unsafe { start(argc, len, main) }
}

fn main(args: &[&[u8]]) -> u64 {
    let target = arg(args, 1);
    let (host, path) = target.split_at(
        target
            .iter()
            .position(|&b| b == b'/')
            .unwrap_or(target.len()),
    );
    let Some((ip, port)) = address(host) else {
        return status(EINVAL);
    };
    let path: &[u8] = if path.is_empty() { b"/" } else { path };
    let buf = map(4096).unwrap_or_else(|| exit(2));
    let times = decimal(arg(args, 2)).unwrap_or(0);
    let start = now_ns();
    for _ in 0..times.max(1) {
        if let Err(error) = get((ip, port), (host, path), buf, times == 0) {
            return status(error);
        }
    }
    if let Some(ns) = (now_ns() - start).checked_div(times) {
        write(CONSOLE, b"bench http-get: ");
        write_u64(CONSOLE, ns);
        write(CONSOLE, b" ns\n");
    }
    0
}

/// One GET of `path` at `host`; prints the body if `print`.
fn get(
    to: ([u8; 4], u16),
    (host, path): (&[u8], &[u8]),
    buf: &mut [u8],
    print: bool,
) -> Result<(), i64> {
    let sock = socket(NET);
    if sock < 0 {
        return Err(sock);
    }
    let sock = sock as u64;
    let result = (|| {
        match connect(sock, to.0, to.1, 0) {
            0 => ok(wait_for(0))?,
            error => return Err(error),
        };
        for part in [b"GET ", path, b" HTTP/1.0\r\nHost: ", host, b"\r\n\r\n"] {
            ok(send(sock, part))?;
        }
        // The head may end in any chunk; what follows it is body.
        let (mut window, mut in_body) = ([0; 4], false);
        loop {
            let n = ok(receive(sock, buf))? as usize;
            if n == 0 {
                return Ok(());
            }
            let mut body = &buf[..n];
            if !in_body {
                let end = body.iter().position(|&b| {
                    window = [window[1], window[2], window[3], b];
                    window == *b"\r\n\r\n"
                });
                let Some(end) = end else { continue };
                (in_body, body) = (true, &body[end + 1..]);
            }
            if print {
                write(CONSOLE, body);
            }
        }
    })();
    close(sock);
    result
}

fn ok(result: i64) -> Result<i64, i64> {
    if result < 0 { Err(result) } else { Ok(result) }
}

/// `a.b.c.d:port`.
fn address(s: &[u8]) -> Option<([u8; 4], u16)> {
    let colon = s.iter().position(|&b| b == b':')?;
    let mut ip = [0; 4];
    let mut parts = s[..colon].split(|&b| b == b'.');
    for octet in &mut ip {
        *octet = u8::try_from(decimal(parts.next()?)?).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((ip, u16::try_from(decimal(&s[colon + 1..])?).ok()?))
}

fn decimal(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0u64, |n, &d| {
        d.is_ascii_digit()
            .then(|| n.checked_mul(10)?.checked_add((d - b'0').into()))?
    })
}
