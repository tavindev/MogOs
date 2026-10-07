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
