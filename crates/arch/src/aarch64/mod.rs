use core::arch::{asm, global_asm};

pub mod gic;
pub mod irq;
mod lock;
mod mmu;
pub mod timer;
mod trap;

pub use lock::*;
pub use mmu::*;
pub use trap::*;

global_asm!(include_str!("boot.s"));

unsafe extern "C" {
    fn aarch64_secondary() -> !;
}

/// PSCI `CPU_ON`'s entry point for a secondary core; the context id holds its per-CPU area (16-byte aligned, also its
/// stack top) in bits 0-47 and its index in bits 48-63. The core turns on its MMU with the boot table, copies the
/// `.percpu` template into the area, sets TPIDR_EL1 (`cpu()`, `PerCpu`) and calls the board's `kmain_secondary`, IRQs
/// masked.
pub fn secondary_entry() -> u64 {
    aarch64_secondary as *const () as u64
}

/// This core's affinity from MPIDR_EL1: Aff0, Aff1 and Aff2 in bits 0-23, Aff3 in bits 32-39 (the layout `GICD_IROUTER`
/// and `ICC_SGI1R_EL1` take).
pub fn mpidr() -> u64 {
    let mpidr: u64;
    // SAFETY: reading MPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack, preserves_flags)) };
    mpidr & 0xff_00ff_ffff
}

/// Microseconds since the virtual counter started.
pub fn uptime_us() -> u64 {
    let (ticks, freq): (u64, u64);
    // SAFETY: reading the generic timer counter and frequency has no side effects; `isb` keeps the read in order.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", "mrs {}, cntfrq_el0", out(reg) ticks, out(reg) freq)
    };
    (ticks as u128 * 1_000_000 / freq as u128) as u64
}
