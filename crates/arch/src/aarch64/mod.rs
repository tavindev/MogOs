use core::arch::{asm, global_asm};

pub mod gic;
pub mod irq;
mod mmu;
pub mod timer;
mod trap;

pub use mmu::*;
pub use trap::*;

global_asm!(include_str!("boot.s"));

/// Microseconds since the virtual counter started.
pub fn uptime_us() -> u64 {
    let (ticks, freq): (u64, u64);
    // SAFETY: reading the generic timer counter and frequency has no side effects; `isb` keeps the read in order.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", "mrs {}, cntfrq_el0", out(reg) ticks, out(reg) freq)
    };
    (ticks as u128 * 1_000_000 / freq as u128) as u64
}
