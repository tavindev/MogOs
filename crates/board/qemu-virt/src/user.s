// User programs, copied into process pages; position independent. x8 = syscall: 0 exit, 1 write, 2 dup, 3 close, 4 map.
// Each starts with init's handles: 0 is the console, 1 the process itself, 2 the boot archive.
.section .rodata.user, "a"
.balign 4

.global user_counter, user_counter_end
user_counter:
    // write(console, kernel address) and write(console, unmapped user address) must both return EFAULT (-14).
    mov x0, #0
    mov x1, #0x40000000
    mov x2, #4
    mov x8, #1
    svc #0
    cmn x0, #14
    b.ne 9f
    mov x0, #0
    movz x1, #0x8000, lsl #16
    movk x1, #1, lsl #32
    mov x2, #4
    mov x8, #1
    svc #0
    cmn x0, #14
    b.ne 9f
    mov x0, #0
    adr x1, 8f
    mov x2, #25
    mov x8, #1
    svc #0
    mov x19, #0
1:  // "A: <x19>\n" on the stack
    movz x9, #0x3a41
    movk x9, #0x3020, lsl #16
    movk x9, #0x0a, lsl #32
    add x9, x9, x19, lsl #24
    str x9, [sp, #-16]!
    mov x0, #0
    mov x1, sp
    mov x2, #5
    mov x8, #1
    svc #0
    add sp, sp, #16
    movz x9, #0x100, lsl #16
2:  subs x9, x9, #1
    b.ne 2b
    add x19, x19, #1
    cmp x19, #10
    b.ne 1b
    mov x0, #0
9:  mov x8, #0
    svc #0
8:  .ascii "A: bad pointers rejected\n"
user_counter_end:

.balign 4
.global user_intruder, user_intruder_end
user_intruder:
    // The counter's code address: mapped only in the counter's address space.
    movz x9, #({USER_BASE} >> 32), lsl #32
    ldr x0, [x9]
    mov x8, #0
    svc #0
user_intruder_end:

.balign 4
.global user_kernel_reader, user_kernel_reader_end
user_kernel_reader:
    // Kernel RAM: in every address space's tables, but EL1-only.
    mov x9, #0x40000000
    ldr x0, [x9]
    mov x8, #0
    svc #0
user_kernel_reader_end:

.balign 4
.global user_bench, user_bench_end
user_bench:
    // Times 100000 write(console, sp, 0) round trips with the virtual counter, prints the ns per round trip.
    mrs x20, cntfrq_el0
    isb
    mrs x21, cntvct_el0
    movz x19, #0x86a0
    movk x19, #1, lsl #16
1:  mov x0, #0
    mov x1, sp
    mov x2, #0
    mov x8, #1
    svc #0
    subs x19, x19, #1
    b.ne 1b
    isb
    mrs x22, cntvct_el0
    sub x22, x22, x21
    mov x9, #10000
    mul x22, x22, x9
    udiv x22, x22, x20
    mov x0, #0
    adr x1, 7f
    mov x2, #9
    mov x8, #1
    svc #0
    // decimal digits of x22, written backwards below sp
    mov x10, sp
    mov x11, sp
    mov x12, #10
2:  udiv x13, x22, x12
    msub x14, x13, x12, x22
    add x14, x14, #'0'
    strb w14, [x11, #-1]!
    mov x22, x13
    cbnz x22, 2b
    mov x0, #0
    mov x1, x11
    sub x2, x10, x11
    mov x8, #1
    svc #0
    mov x0, #0
    adr x1, 6f
    mov x2, #15
    mov x8, #1
    svc #0
    mov x0, #0
    mov x8, #0
    svc #0
7:  .ascii "syscall: "
6:  .ascii " ns/round-trip\n"
user_bench_end:

// x0 = write(\handle, \str, \len)
.macro write handle, str, len
    mov x0, \handle
    adr x1, \str
    mov x2, #\len
    mov x8, #1
    svc #0
.endm

.balign 4
.global user_handles, user_handles_end
user_handles:
    // The console handle writes and returns the length.
    write #0, 1f, 20
    cmp x0, #20
    b.ne 9f
    // x19 = dup(console, duplicate): writing through it is EACCES (-13).
    mov x0, #0
    mov x1, #8
    mov x8, #2
    svc #0
    mov x19, x0
    write x19, 1f, 20
    cmn x0, #13
    b.ne 9f
    write #0, 2f, 29
    // After close(x19), writing through it is EBADF (-9).
    mov x0, x19
    mov x8, #3
    svc #0
    cbnz x0, 9f
    write x19, 1f, 20
    cmn x0, #9
    b.ne 9f
    write #0, 3f, 24
    // x20 = dup(console, write) reuses x19's entry with a new generation; x19 stays EBADF.
    mov x0, #0
    mov x1, #2
    mov x8, #2
    svc #0
    mov x20, x0
    cmp w19, w20
    b.ne 9f
    cmp x19, x20
    b.eq 9f
    write x19, 1f, 20
    cmn x0, #9
    b.ne 9f
    write x20, 4f, 23
9:  mov x0, #0
    mov x8, #0
    svc #0
1:  .ascii "H: console write ok\n"
2:  .ascii "H: dup without write: EACCES\n"
3:  .ascii "H: closed handle: EBADF\n"
4:  .ascii "H: stale handle: EBADF\n"
user_handles_end:

.balign 4
.global user_budget, user_budget_end
user_budget:
    // map(0x10001), one byte over MAX_MAP, is EINVAL (-22).
    movz x0, #1
    movk x0, #1, lsl #16
    mov x8, #4
    svc #0
    cmn x0, #22
    b.ne 9f
    // map of the 16 frames left after the fixed 9 needs 17 with its level-3 table: ENOMEM, the 15 mapped pages undone.
    mov x0, #0x10000
    mov x8, #4
    svc #0
    cmn x0, #12
    b.ne 9f
    // x19 = pages from map(4096) until it fails; each must read zero and take a write.
    mov x19, #0
1:  mov x0, #4096
    mov x8, #4
    svc #0
    tbnz x0, #63, 2f
    ldr x9, [x0]
    cbnz x9, 9f
    str x0, [x0]
    add x19, x19, #1
    b 1b
2:  cmn x0, #12
    b.ne 9f
    write #0, 7f, 16
    // decimal digits of x19, written backwards below sp
    mov x10, sp
    mov x11, sp
    mov x12, #10
3:  udiv x13, x19, x12
    msub x14, x13, x12, x19
    add x14, x14, #'0'
    strb w14, [x11, #-1]!
    mov x19, x13
    cbnz x19, 3b
    mov x0, #0
    mov x1, x11
    sub x2, x10, x11
    mov x8, #1
    svc #0
    write #0, 6f, 7
    write #0, 5f, 17
9:  mov x0, #0
    mov x8, #0
    svc #0
7:  .ascii "M: ENOMEM after "
6:  .ascii " pages\n"
5:  .ascii "M: still running\n"
user_budget_end:
