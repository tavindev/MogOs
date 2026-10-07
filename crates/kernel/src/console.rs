//! Console input: a line discipline over the bytes the UART receives. One line at a time; one reader at a time.

/// Line capacity, `\n` included.
pub const LINE: usize = 256;

pub struct Line {
    buf: [u8; LINE],
    len: usize,
    /// Enter was pressed; the line waits for `read`, and input is dropped until then.
    ready: bool,
}

impl Line {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE],
            len: 0,
            ready: false,
        }
    }

    /// Takes one received byte, passing what to echo to `echo`; true if it completed a line. Printable ASCII is
    /// appended (dropped once the line is full), backspace (0x7f or 0x08) erases one char, `\r` or `\n` ends the line
    /// with `\n`; anything else is ignored.
    pub fn push(&mut self, byte: u8, mut echo: impl FnMut(&[u8])) -> bool {
        if self.ready {
            return false;
        }
        match byte {
            b'\r' | b'\n' => {
                self.buf[self.len] = b'\n';
                self.len += 1;
                self.ready = true;
                echo(b"\n");
            }
            0x7f | 0x08 if self.len > 0 => {
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
        self.ready
    }

    /// Moves the completed line into `out`, dropping what does not fit; returns its length, or `None` (wait) until
    /// Enter. An empty `out` returns 0 at once.
    pub fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        if out.is_empty() {
            return Some(0);
        }
        if !self.ready {
            return None;
        }
        let n = self.len.min(out.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        (self.len, self.ready) = (0, false);
        Some(n)
    }
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}
