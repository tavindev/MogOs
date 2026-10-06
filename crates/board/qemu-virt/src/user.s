// User programs, copied into process pages; position independent. x8 = syscall, 0 exit, 1 print.
.section .rodata.user, "a"
.balign 4

.global user_counter, user_counter_end
user_counter:
    // print(kernel address) and print(unmapped user address) must both return EFAULT (-14).
    mov x0, #0x40000000
    mov x1, #4
    mov x8, #1
    svc #0
    cmn x0, #14
    b.ne 9f
    movz x0, #0x8000, lsl #16
    movk x0, #1, lsl #32
    mov x1, #4
    mov x8, #1
    svc #0
    cmn x0, #14
    b.ne 9f
    adr x0, 8f
    mov x1, #25
    mov x8, #1
    svc #0
    mov x19, #0
1:  // "A: <x19>\n" on the stack
    movz x9, #0x3a41
    movk x9, #0x3020, lsl #16
    movk x9, #0x0a, lsl #32
    add x9, x9, x19, lsl #24
    str x9, [sp, #-16]!
    mov x0, sp
    mov x1, #5
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
    movz x9, #1, lsl #32
    ldr x0, [x9]
    mov x8, #0
    svc #0
user_intruder_end:
