// The kernel's memcpy and memmove, 16 bytes per unaligned ldp/stp (free on Normal memory), no FP/SIMD registers.
// With the MMU off every access is Device and must be aligned to its size: it is whenever dst and src are 8-aligned.

.section .text.memmove
.global memcpy
.global memmove
.type memcpy, %function
.type memmove, %function
memcpy:
memmove:
    // dst in [src, src + n) copies from the end, so no byte is overwritten before it is read (n = 0 copies nothing).
    sub x3, x0, x1
    cmp x3, x2
    b.lo 2f
    mov x4, x0
    subs x5, x2, #16
    b.lo 1f
0:
    ldp x6, x7, [x1], #16
    stp x6, x7, [x4], #16
    subs x5, x5, #16
    b.hs 0b
1:
    tbz x2, #3, 1f
    ldr x6, [x1], #8
    str x6, [x4], #8
1:
    tbz x2, #2, 1f
    ldr w6, [x1], #4
    str w6, [x4], #4
1:
    tbz x2, #1, 1f
    ldrh w6, [x1], #2
    strh w6, [x4], #2
1:
    tbz x2, #0, 1f
    ldrb w6, [x1]
    strb w6, [x4]
1:
    ret
2:
    add x1, x1, x2
    add x4, x0, x2
    subs x5, x2, #16
    b.lo 1f
0:
    ldp x6, x7, [x1, #-16]!
    stp x6, x7, [x4, #-16]!
    subs x5, x5, #16
    b.hs 0b
1:
    tbz x2, #3, 1f
    ldr x6, [x1, #-8]!
    str x6, [x4, #-8]!
1:
    tbz x2, #2, 1f
    ldr w6, [x1, #-4]!
    str w6, [x4, #-4]!
1:
    tbz x2, #1, 1f
    ldrh w6, [x1, #-2]!
    strh w6, [x4, #-2]!
1:
    tbz x2, #0, 1f
    ldrb w6, [x1, #-1]
    strb w6, [x4, #-1]
1:
    ret
.size memcpy, . - memcpy
.size memmove, . - memmove
