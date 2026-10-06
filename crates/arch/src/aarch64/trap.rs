use core::arch::{asm, global_asm};

/// Registers saved on exception entry; the layout is fixed by the vector asm below.
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    elr: u64,
    spsr: u64,
    /// Not touched by the vector asm: the kernel never uses SP_EL0, so only `switch_sp_el0` saves and loads it.
    sp_el0: u64,
}

const _: () = assert!(size_of::<TrapFrame>() == 272);

// Each of the 16 entries saves x0/x1, puts its index in x1, and joins the common path.
global_asm!(
    r#"
.macro VECTOR index
    .balign 0x80
    sub sp, sp, #272
    stp x0, x1, [sp]
    mov x1, #\index
    b .Ltrap
.endm

.section .text.vectors, "ax"
.balign 2048
.global aarch64_vectors
aarch64_vectors:
    VECTOR 0
    VECTOR 1
    VECTOR 2
    VECTOR 3
    VECTOR 4
    VECTOR 5
    VECTOR 6
    VECTOR 7
    VECTOR 8
    VECTOR 9
    VECTOR 10
    VECTOR 11
    VECTOR 12
    VECTOR 13
    VECTOR 14
    VECTOR 15

.Ltrap:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    mrs x2, elr_el1
    stp x30, x2, [sp, #240]
    mrs x2, spsr_el1
    str x2, [sp, #256]
    mov x0, sp
    bl aarch64_exception
    mov sp, x0
    ldp x30, x2, [sp, #240]
    msr elr_el1, x2
    ldr x2, [sp, #256]
    msr spsr_el1, x2
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldp x0, x1, [sp]
    add sp, sp, #272
    eret
"#
);

const KINDS: [&str; 4] = ["sync", "irq", "fiq", "serror"];
const SOURCES: [&str; 4] = [
    "current EL with SP0",
    "current EL with SPx",
    "lower EL (AArch64)",
    "lower EL (AArch32)",
];
const SYNC_CURRENT_SPX: u64 = 4;
const IRQ_CURRENT_SPX: u64 = 5;
const SYNC_LOWER64: u64 = 8;
const IRQ_LOWER64: u64 = 9;
const EC_SVC64: u64 = 0x15;
const EC_BRK64: u64 = 0x3c;
/// EL1h with IRQs unmasked; D, A and F masked.
const SPSR_EL1H_IRQ_ON: u64 = 0x345;
/// EL0t with IRQs unmasked; D, A and F masked.
const SPSR_EL0T_IRQ_ON: u64 = 0x340;

unsafe extern "C" {
    /// The board's scheduler: saves the yielding task's frame address and returns the next task's.
    fn task_switch(frame: usize) -> usize;
}

/// Points `VBAR_EL1` at this crate's vector table.
pub fn install_vectors() {
    // SAFETY: `aarch64_vectors` is a complete, 2 KiB aligned EL1 vector table.
    unsafe {
        asm!(
            "adrp {t}, aarch64_vectors",
            "add {t}, {t}, :lo12:aarch64_vectors",
            "msr vbar_el1, {t}",
            "isb",
            t = out(reg) _,
        )
    }
}

/// Executes `brk #0`, which the handler skips; returning proves it was caught. Call after `install_vectors`.
pub fn breakpoint_self_test() {
    // SAFETY: the installed sync handler skips `brk #0` and resumes after it.
    unsafe { asm!("brk #0", clobber_abi("C")) };
}

// SAFETY: the board defines `board_irq` with this signature.
unsafe extern "C" {
    /// Handles the pending IRQ; returns the frame to resume, `frame` or the next task's. Requires IRQs masked.
    fn board_irq(frame: usize) -> usize;
    /// Runs the syscall a process made with `svc`; returns the frame to resume. Requires IRQs masked.
    fn board_syscall(frame: &mut TrapFrame) -> usize;
    /// Kills the process whose instruction faulted at EL0; returns the next task's frame. Requires IRQs masked.
    fn board_user_fault(frame: usize) -> usize;
}

/// Writes a frame just below `stack_top` that starts `entry(arg)` at EL1h with IRQs unmasked; returns its address for the scheduler.
///
/// # Safety
///
/// `stack_top` must be 16-byte aligned, with the memory below it a fresh stack owned by the new task.
pub unsafe fn new_task(stack_top: usize, entry: extern "C" fn(usize) -> !, arg: usize) -> usize {
    let frame = (stack_top - size_of::<TrapFrame>()) as *mut TrapFrame;
    let mut x = [0; 31];
    x[0] = arg as u64;
    // SAFETY: the caller guarantees the bytes below `stack_top` are ours to write.
    unsafe {
        frame.write(TrapFrame {
            x,
            elr: entry as usize as u64,
            spsr: SPSR_EL1H_IRQ_ON,
            sp_el0: 0,
        })
    };
    frame as usize
}

/// Writes a frame just below `stack_top` (the process's kernel stack) that starts at user address `entry`
/// at EL0 with IRQs unmasked and SP_EL0 = `sp`; returns its address for the scheduler.
///
/// # Safety
///
/// `stack_top` must be 16-byte aligned, with the memory below it a fresh stack owned by the new task.
pub unsafe fn new_user_task(stack_top: usize, entry: u64, sp: u64) -> usize {
    let frame = (stack_top - size_of::<TrapFrame>()) as *mut TrapFrame;
    // SAFETY: the caller guarantees the bytes below `stack_top` are ours to write.
    unsafe {
        frame.write(TrapFrame {
            x: [0; 31],
            elr: entry,
            spsr: SPSR_EL0T_IRQ_ON,
            sp_el0: sp,
        })
    };
    frame as usize
}

/// Saves SP_EL0 into the frame at `from` and loads it from the frame at `to`; needed only when switching
/// between address spaces, since user tasks are the only users of SP_EL0.
///
/// # Safety
///
/// `from` and `to` must be trap frames: saved by the trap path or written by `new_task`/`new_user_task`.
pub unsafe fn switch_sp_el0(from: usize, to: usize) {
    let sp: u64;
    // SAFETY: SP_EL0 is not the running stack (EL1h), so reading it has no effect.
    unsafe { asm!("mrs {}, sp_el0", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    // SAFETY: the caller guarantees `from` is a trap frame.
    unsafe { (*(from as *mut TrapFrame)).sp_el0 = sp };
    // SAFETY: the caller guarantees `to` is a trap frame.
    let sp = unsafe { (*(to as *const TrapFrame)).sp_el0 };
    // SAFETY: as above, writing SP_EL0 does not move the running stack.
    unsafe { asm!("msr sp_el0, {}", in(reg) sp, options(nomem, nostack, preserves_flags)) };
}

/// Executes `svc #0`: switches to the next task; returns when the scheduler picks this one again.
pub fn yield_now() {
    // SAFETY: the sync handler saves and restores every register around the switch.
    unsafe { asm!("svc #0") };
}

#[unsafe(no_mangle)]
extern "C" fn aarch64_exception(frame: &mut TrapFrame, index: u64) -> usize {
    if index == IRQ_CURRENT_SPX || index == IRQ_LOWER64 {
        // SAFETY: exception entry masked IRQs.
        return unsafe { board_irq(frame as *mut TrapFrame as usize) };
    }
    let esr: u64;
    // SAFETY: reading ESR_EL1 has no side effects.
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr) };
    let ec = (esr >> 26) & 0x3f;
    if index == SYNC_LOWER64 && ec == EC_SVC64 {
        // SAFETY: the board defines `board_syscall`; exception entry masked IRQs.
        return unsafe { board_syscall(frame) };
    }
    if index == SYNC_LOWER64 {
        // SAFETY: the board defines `board_user_fault`; exception entry masked IRQs.
        return unsafe { board_user_fault(frame as *mut TrapFrame as usize) };
    }
    if index == SYNC_CURRENT_SPX && ec == EC_SVC64 && esr & 0xffff == 0 {
        // SAFETY: the board defines `task_switch`; exception entry masked IRQs.
        return unsafe { task_switch(frame as *mut TrapFrame as usize) };
    }
    if index == SYNC_CURRENT_SPX && ec == EC_BRK64 && esr & 0xffff == 0 {
        frame.elr += 4;
        return frame as *mut TrapFrame as usize;
    }
    let far: u64;
    // SAFETY: reading FAR_EL1 has no side effects.
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    panic!(
        "unhandled {} exception from {}: ESR_EL1={esr:#x} FAR_EL1={far:#x} ELR_EL1={:#x}",
        KINDS[index as usize % 4],
        SOURCES[index as usize / 4],
        frame.elr
    );
}
