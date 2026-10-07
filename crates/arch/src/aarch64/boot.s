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

// PSCI CPU_ON entry of a secondary core, MMU off: x0 (the context id) is its stack top.
.global aarch64_secondary
aarch64_secondary:
    bl aarch64_mmu_on
    mov sp, x0
    mrs x1, mpidr_el1
    and x1, x1, #0xff
    msr tpidr_el1, x1
    bl kmain_secondary
4:  wfe
    b 4b
