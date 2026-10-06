use core::fmt;

use mm::PhysAddr;

/// PL011 UART on QEMU `virt` (identity mapped). QEMU accepts writes to DR without initialization.
#[derive(Clone)]
pub struct Uart {
    base: PhysAddr,
}

impl Uart {
    pub const fn new(base: PhysAddr) -> Self {
        Self { base }
    }

    pub fn put(&mut self, byte: u8) {
        // SAFETY: `base` is the MMIO data register of a PL011 on this board.
        unsafe { (self.base.0 as *mut u32).write_volatile(byte as u32) }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}
