.section .text.boot
.global _start
_start:
    // Rust for this target emits FP/SIMD; stop CPACR_EL1.FPEN from trapping it.
    mov x1, #(3 << 20)
    msr cpacr_el1, x1
    isb
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
