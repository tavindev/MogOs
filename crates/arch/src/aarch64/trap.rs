use core::arch::{asm, global_asm};

/// Registers saved on exception entry; the layout is fixed by the vector asm below.
#[repr(C, align(16))]
pub struct TrapFrame {
    pub x: [u64; 31],
    elr: u64,
    spsr: u64,
    /// Not touched by the vector asm: the kernel never uses SP_EL0 or TPIDR_EL0, so only `switch_el0_regs`
    /// saves and loads them.
    sp_el0: u64,
    tpidr_el0: u64,
}

const _: () = assert!(size_of::<TrapFrame>() == 288);

impl TrapFrame {
    /// Rewinds to the `svc` that trapped, so it runs again when this frame resumes.
    pub fn restart(&mut self) {
        self.elr -= 4;
    }
}

// The vector tables, 2 KiB apart in `spec::TABLES` order. Each entry saves x0/x1, puts its index in x1, and joins the
// common path; a lower-EL entry (8-15) first runs its table's Spectre-BHB mitigation, before any branch.
global_asm!(
    r#"
// kind: 0 plain, 1 clearbhb, 2 and 3 firmware workaround 3 by hvc and smc, 4 and 5 a loop of k with `dsb nsh; isb`
// and with `sb`.
.macro MITIGATE kind, k
    .if \kind == 1
    hint #22 // clrbhb
    isb
    .elseif \kind == 2 || \kind == 3
    // SMCCC 1.1 clobbers x0-x3 only; x2 and x3 go back from their frame slots.
    stp x2, x3, [sp, #16]
    movz w0, #0x3fff
    movk w0, #0x8000, lsl #16
    .if \kind == 2
    hvc #0
    .else
    smc #0
    .endif
    ldp x2, x3, [sp, #16]
    .elseif \kind >= 4
    mov x0, #\k
1:  b . + 4
    subs x0, x0, #1
    b.ne 1b
    .if \kind == 4
    dsb nsh
    isb
    .else
    .inst 0xd50330ff // sb
    .endif
    .endif
.endm

.macro VECTOR index, kind, k
    .balign 0x80
    sub sp, sp, #288
    stp x0, x1, [sp]
    .if \index >= 8
    MITIGATE \kind, \k
    .endif
    mov x1, #\index
    b .Ltrap
.endm

.macro TABLE kind, k=0
    .balign 2048
    .irp index, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
    VECTOR \index, \kind, \k
    .endr
.endm

.section .text.vectors, "ax"
.balign 2048
.global aarch64_vectors
aarch64_vectors:
    TABLE 0
    TABLE 1
    TABLE 2
    TABLE 3
    .irp k, 8, 11, 24, 32, 38, 132
    TABLE 4, \k
    TABLE 5, \k
    .endr

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
    // Only once off the old stack: with the board's kernel lock free, another core may run the task that owns it.
1:  cbz x1, 3f
    cmp x1, #1
    b.ne 2f
    bl board_unlock
    b 3f
    // Work the hook left: it may pick another frame, which holds the lock again.
2:  mov x0, sp
    bl board_unlock_work
    mov sp, x0
    b 1b
3:
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
    add sp, sp, #288
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
    /// The board's scheduler: saves the yielding task's frame address and returns the next task's; entered and left like
    /// the hooks below.
    fn task_switch(frame: usize) -> Resume;
}

/// What a trap hook returns: the frame to resume, and whether the hook holds the board's kernel lock, which the trap
/// exit then releases once it has moved to that frame: 0 not held, 1 held (`board_unlock`), 2 held with work left
/// (`board_unlock_work`, which returns the frame to resume after it, as a hook does).
#[repr(C)]
pub struct Resume {
    frame: usize,
    locked: usize,
}

impl Resume {
    /// Resumes `frame`, the hook holding the kernel lock, with `work` for `board_unlock_work`.
    pub fn locked(frame: usize, work: bool) -> Self {
        Self {
            frame,
            locked: 1 + work as usize,
        }
    }

    /// Resumes `frame`, the hook holding no lock.
    pub fn unlocked(frame: usize) -> Self {
        Self { frame, locked: 0 }
    }
}

/// Executes `brk #0`, which the handler skips; returning proves it was caught. Call after `install_vectors`.
pub fn breakpoint_self_test() {
    // SAFETY: the installed sync handler skips `brk #0` and resumes after it.
    unsafe { asm!("brk #0", clobber_abi("C")) };
}

// SAFETY: the board defines these with these signatures. Each is entered with IRQs masked and says in its `Resume`
// whether it returns holding the board's kernel lock; one that switched tasks always does.
unsafe extern "C" {
    /// Handles the pending IRQ; resumes `frame` or the next task's.
    fn board_irq(frame: usize) -> Resume;
    /// Runs the syscall a process made with `svc`; resumes `frame` or the next task's.
    fn board_syscall(frame: &mut TrapFrame) -> Resume;
    /// Kills the process whose instruction faulted at EL0 with exception class `ec` at address `far`; resumes the next
    /// task's frame.
    fn board_user_fault(frame: usize, ec: u64, far: u64) -> Resume;
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
            tpidr_el0: 0,
        })
    };
    frame as usize
}

/// Writes a frame just below `stack_top` (the thread's kernel stack) that starts at user address `entry`
/// at EL0 with IRQs unmasked, SP_EL0 = `sp`, TPIDR_EL0 = `tls` and x0-x2 = `args`; returns its address for the
/// scheduler.
///
/// # Safety
///
/// `stack_top` must be 16-byte aligned, with the memory below it a fresh stack owned by the new task.
pub unsafe fn new_user_task(
    stack_top: usize,
    entry: u64,
    (sp, tls): (u64, u64),
    args: [u64; 3],
) -> usize {
    let frame = (stack_top - size_of::<TrapFrame>()) as *mut TrapFrame;
    let mut x = [0; 31];
    x[..3].copy_from_slice(&args);
    // SAFETY: the caller guarantees the bytes below `stack_top` are ours to write.
    unsafe {
        frame.write(TrapFrame {
            x,
            elr: entry,
            spsr: SPSR_EL0T_IRQ_ON,
            sp_el0: sp,
            tpidr_el0: tls,
        })
    };
    frame as usize
}

/// Saves SP_EL0 and TPIDR_EL0 into the frame at `from` and loads them from the frame at `to`; needed on every
/// switch with a user task on either side, since each thread has its own and kernel tasks never use them.
///
/// # Safety
///
/// `from` and `to` must be trap frames: saved by the trap path or written by `new_task`/`new_user_task`.
pub unsafe fn switch_el0_regs(from: usize, to: usize) {
    let (sp, tp): (u64, u64);
    // SAFETY: SP_EL0 is not the running stack (EL1h) and the kernel never uses TPIDR_EL0, so reading them has no effect.
    unsafe {
        asm!("mrs {}, sp_el0", "mrs {}, tpidr_el0", out(reg) sp, out(reg) tp, options(nomem, nostack, preserves_flags))
    };
    // SAFETY: the caller guarantees `from` is a trap frame.
    let from = unsafe { &mut *(from as *mut TrapFrame) };
    (from.sp_el0, from.tpidr_el0) = (sp, tp);
    // SAFETY: the caller guarantees `to` is a trap frame.
    let to = unsafe { &*(to as *const TrapFrame) };
    // SAFETY: as above, writing them does not move the running stack or touch kernel state.
    unsafe {
        asm!("msr sp_el0, {}", "msr tpidr_el0, {}", in(reg) to.sp_el0, in(reg) to.tpidr_el0, options(nomem, nostack, preserves_flags))
    };
}

/// Executes `svc #0`: switches to the next task; returns when the scheduler picks this one again.
pub fn yield_now() {
    // SAFETY: the sync handler saves and restores every register around the switch.
    unsafe { asm!("svc #0") };
}

#[unsafe(no_mangle)]
extern "C" fn aarch64_exception(frame: &mut TrapFrame, index: u64) -> Resume {
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
        return unsafe { board_user_fault(frame as *mut TrapFrame as usize, ec, far_el1()) };
    }
    if index == SYNC_CURRENT_SPX && ec == EC_SVC64 && esr & 0xffff == 0 {
        // SAFETY: the board defines `task_switch`; exception entry masked IRQs.
        return unsafe { task_switch(frame as *mut TrapFrame as usize) };
    }
    if index == SYNC_CURRENT_SPX && ec == EC_BRK64 && esr & 0xffff == 0 {
        frame.elr += 4;
        return Resume::unlocked(frame as *mut TrapFrame as usize);
    }
    panic!(
        "unhandled {} exception from {}: ESR_EL1={esr:#x} FAR_EL1={far:#x} ELR_EL1={:#x}",
        KINDS[index as usize % 4],
        SOURCES[index as usize / 4],
        frame.elr,
        far = far_el1(),
    );
}

fn far_el1() -> u64 {
    let far: u64;
    // SAFETY: reading FAR_EL1 has no side effects.
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    far
}
