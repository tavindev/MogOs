use core::fmt;

use mm::PhysAddr;

const FR: u64 = 0x18;
const FR_RXFE: u32 = 1 << 4;
const CR: u64 = 0x30;
const CR_UARTEN_TXE_RXE: u32 = 0x301;
const IMSC: u64 = 0x38;
const IMSC_RXIM: u32 = 1 << 4;

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

    /// Turns on the receiver and its interrupt.
    pub fn enable_rx_irq(&mut self) {
        for (offset, value) in [(CR, CR_UARTEN_TXE_RXE), (IMSC, IMSC_RXIM)] {
            // SAFETY: `base` is a PL011 on this board; CR and IMSC are its registers.
            unsafe { ((self.base.0 + offset) as *mut u32).write_volatile(value) }
        }
    }

    /// The next received byte, if any; reading the last one clears the receive interrupt.
    pub fn get(&mut self) -> Option<u8> {
        // SAFETY: `base` is a PL011 on this board; FR is its flag register.
        let flags = unsafe { ((self.base.0 + FR) as *const u32).read_volatile() };
        // SAFETY: `base` is the MMIO data register of a PL011 on this board.
        (flags & FR_RXFE == 0).then(|| unsafe { (self.base.0 as *const u32).read_volatile() } as u8)
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
