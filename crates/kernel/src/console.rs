//! Console input: a line discipline over the bytes the UART receives. One reader at a time.

/// Buffer capacity: completed lines typed ahead plus the line being edited, each `\n` included.
pub const LINE: usize = 256;

pub struct Line {
    buf: [u8; LINE],
    len: usize,
    /// End of the completed lines in `buf`; the line being edited follows.
    done: usize,
}

impl Line {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE],
            len: 0,
            done: 0,
        }
    }

    /// Takes one received byte, passing what to echo to `echo`; true if it completed a line. Printable ASCII is
    /// appended (dropped once the buffer is full), backspace (0x7f or 0x08) erases one char of the line being edited,
    /// `\r` or `\n` ends it with `\n`; anything else is ignored.
    pub fn push(&mut self, byte: u8, mut echo: impl FnMut(&[u8])) -> bool {
        match byte {
            b'\r' | b'\n' if self.len < LINE => {
                self.buf[self.len] = b'\n';
                self.len += 1;
                self.done = self.len;
                echo(b"\n");
                return true;
            }
            0x7f | 0x08 if self.len > self.done => {
                self.len -= 1;
                echo(b"\x08 \x08");
            }
            0x20..=0x7e if self.len < LINE - 1 => {
                self.buf[self.len] = byte;
                self.len += 1;
                echo(&[byte]);
            }
            _ => {}
        }
        false
    }

    /// Moves the first completed line into `out`, dropping what does not fit; returns its length, or `None` (wait)
    /// until one is completed. An empty `out` returns 0 at once.
    pub fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        if out.is_empty() {
            return Some(0);
        }
        let end = self.buf[..self.done].iter().position(|&b| b == b'\n')? + 1;
        let n = end.min(out.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.copy_within(end..self.len, 0);
        self.len -= end;
        self.done -= end;
        Some(n)
    }
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}
