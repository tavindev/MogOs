.section .text.boot
.global _start
_start:
    msr tpidr_el1, xzr
    ldr x1, =__stack_top
    mov sp, x1
    ldr x1, =__bss_start
    ldr x2, =__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:  bl kmain
3:  wfe
    b 3b

// PSCI CPU_ON entry of a secondary core, MMU off: x0 (the context id) holds its per-CPU area, which is also its stack
// top, in bits 0-47 and its index in bits 48-63. It copies the `.percpu` template into the area, then sets
// TPIDR_EL1 to the area's offset from the template (bits 0-47) and the index.
.global aarch64_secondary
aarch64_secondary:
    bl aarch64_mmu_on
    and x1, x0, #0xffffffffffff
    mov sp, x1
    adrp x2, __percpu_start
    add x2, x2, :lo12:__percpu_start
    adrp x3, __percpu_end
    add x3, x3, :lo12:__percpu_end
    sub x4, x1, x2
    mov x5, x1
1:  cmp x2, x3
    b.hs 2f
    ldp x6, x7, [x2], #16
    stp x6, x7, [x5], #16
    b 1b
2:  and x4, x4, #0xffffffffffff
    and x0, x0, #0xffff000000000000
    orr x0, x0, x4
    msr tpidr_el1, x0
    bl kmain_secondary
4:  wfe
    b 4b
