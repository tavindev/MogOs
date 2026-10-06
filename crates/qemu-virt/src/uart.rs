use core::fmt;

/// PL011 UART on QEMU `virt`. QEMU accepts writes to DR without initialization.
pub struct Uart {
    base: usize,
}

impl Uart {
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    pub fn put(&mut self, byte: u8) {
        // SAFETY: `base` is the MMIO data register of a PL011 on this board.
        unsafe { (self.base as *mut u32).write_volatile(byte as u32) }
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
        Ok(())
    }
}
