use core::arch::asm;

/// Arms the EL1 virtual timer to raise its interrupt `us` microseconds from now.
pub fn arm(us: u64) {
    let freq: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effects.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) freq) };
    // SAFETY: programming the virtual timer only affects its own interrupt; `isb` completes it before a following EOI.
    unsafe {
        asm!(
            "msr cntv_tval_el0, {tval}",
            "msr cntv_ctl_el0, {enable}",
            "isb",
            tval = in(reg) freq * us / 1_000_000,
            enable = in(reg) 1u64,
        )
    };
}
