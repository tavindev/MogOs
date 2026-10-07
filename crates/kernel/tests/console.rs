use kernel::console::{LINE, Line};

/// Feeds `input` to `line`; returns the echo and whether the last byte completed a line.
fn feed(line: &mut Line, input: &[u8]) -> (Vec<u8>, bool) {
    let mut echo = Vec::new();
    let mut done = false;
    for &byte in input {
        done = line.push(byte, |e| echo.extend_from_slice(e));
    }
    (echo, done)
}

fn read(line: &mut Line) -> Option<Vec<u8>> {
    let mut out = [0; LINE];
    line.read(&mut out).map(|n| out[..n].to_vec())
}

#[test]
fn echoes_and_delivers_the_line_on_enter() {
    let mut line = Line::new();
    assert_eq!(feed(&mut line, b"hi"), (b"hi".to_vec(), false));
    assert_eq!(read(&mut line), None);
    assert_eq!(feed(&mut line, b"\r"), (b"\n".to_vec(), true));
    assert_eq!(read(&mut line).as_deref(), Some(&b"hi\n"[..]));
    assert_eq!(read(&mut line), None);
    assert_eq!(feed(&mut line, b"x\n"), (b"x\n".to_vec(), true));
    assert_eq!(read(&mut line).as_deref(), Some(&b"x\n"[..]));
}

#[test]
fn backspace_erases_one_char() {
    let mut line = Line::new();
    let (echo, _) = feed(&mut line, b"\x7fab\x7f\x08\x08c\r");
    assert_eq!(echo, b"ab\x08 \x08\x08 \x08c\n");
    assert_eq!(read(&mut line).as_deref(), Some(&b"c\n"[..]));
}

#[test]
fn ignores_control_bytes() {
    let mut line = Line::new();
    assert_eq!(feed(&mut line, b"a\x1b\x00\tb"), (b"ab".to_vec(), false));
}

#[test]
fn a_full_line_drops_more_chars_but_still_takes_enter() {
    let mut line = Line::new();
    let (echo, _) = feed(&mut line, &[b'a'; LINE + 5]);
    assert_eq!(echo.len(), LINE - 1);
    assert_eq!(feed(&mut line, b"\r"), (b"\n".to_vec(), true));
    let mut expected = vec![b'a'; LINE - 1];
    expected.push(b'\n');
    assert_eq!(read(&mut line), Some(expected));
}

#[test]
fn input_is_dropped_while_a_line_is_pending() {
    let mut line = Line::new();
    feed(&mut line, b"a\r");
    assert_eq!(feed(&mut line, b"b\r"), (Vec::new(), false));
    assert_eq!(read(&mut line).as_deref(), Some(&b"a\n"[..]));
}

#[test]
fn a_short_read_drops_the_rest_and_an_empty_one_takes_nothing() {
    let mut line = Line::new();
    feed(&mut line, b"hello\r");
    assert_eq!(line.read(&mut []), Some(0));
    let mut out = [0; 3];
    assert_eq!(line.read(&mut out), Some(3));
    assert_eq!(&out, b"hel");
    assert_eq!(read(&mut line), None);
}
