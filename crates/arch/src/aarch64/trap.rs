use core::arch::{asm, global_asm};

/// Registers saved on exception entry; the layout is fixed by the vector asm below.
#[repr(C)]
struct TrapFrame {
    x: [u64; 31],
    elr: u64,
    spsr: u64,
    _pad: u64,
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
const EC_BRK64: u64 = 0x3c;

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

#[unsafe(no_mangle)]
extern "C" fn aarch64_exception(frame: &mut TrapFrame, index: u64) {
    let esr: u64;
    // SAFETY: reading ESR_EL1 has no side effects.
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr) };
    if index == SYNC_CURRENT_SPX && (esr >> 26) & 0x3f == EC_BRK64 && esr & 0xffff == 0 {
        frame.elr += 4;
        return;
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
