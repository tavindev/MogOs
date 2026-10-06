use core::arch::asm;

/// DAIF as it was before `disable`.
#[must_use = "dropping it leaves IRQs masked"]
pub struct State(u64);

/// Masks IRQs and returns the previous mask state.
pub fn disable() -> State {
    let daif: u64;
    // SAFETY: saving DAIF and setting its I bit only masks IRQs; no `nomem`, so it orders memory accesses like a lock.
    unsafe {
        asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nostack, preserves_flags))
    };
    State(daif)
}

/// Puts back the IRQ mask saved by `disable`.
pub fn restore(state: State) {
    // SAFETY: writes back a DAIF value read by `disable`; no `nomem`, so it orders memory accesses like an unlock.
    unsafe { asm!("msr daif, {}", in(reg) state.0, options(nostack, preserves_flags)) };
}

/// With IRQs masked, sleeps until an interrupt is pending, then briefly unmasks so its handler runs.
pub fn wait() {
    // SAFETY: `wfi` wakes on a pending IRQ even while masked; the `isb` makes sure it is taken before re-masking.
    unsafe {
        asm!(
            "wfi",
            "msr daifclr, #2",
            "isb",
            "msr daifset, #2",
            options(nostack)
        )
    };
}
