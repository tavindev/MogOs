// MogOs has no fork: vfork saves the caller's registers and returns 0 in "child mode" on the same stack; the child's
// execve (a native spawn) or _exit then returns its pid from here through __mog_vfork_resume.
.global vfork
.type vfork,%function
vfork:
	adrp x16, __mog_vfork_child
	ldr w17, [x16, :lo12:__mog_vfork_child]
	cbnz w17, 1f
	adrp x16, __mog_vfork_jb
	add x16, x16, :lo12:__mog_vfork_jb
	stp x19, x20, [x16,#0]
	stp x21, x22, [x16,#16]
	stp x23, x24, [x16,#32]
	stp x25, x26, [x16,#48]
	stp x27, x28, [x16,#64]
	stp x29, x30, [x16,#80]
	mov x17, sp
	str x17, [x16,#96]
	b __mog_vfork_enter
1:	mov x0, #-11 // EAGAIN
	.hidden __syscall_ret
	b __syscall_ret

.global __mog_vfork_resume
.hidden __mog_vfork_resume
.type __mog_vfork_resume,%function
__mog_vfork_resume:
	adrp x16, __mog_vfork_jb
	add x16, x16, :lo12:__mog_vfork_jb
	ldp x19, x20, [x16,#0]
	ldp x21, x22, [x16,#16]
	ldp x23, x24, [x16,#32]
	ldp x25, x26, [x16,#48]
	ldp x27, x28, [x16,#64]
	ldp x29, x30, [x16,#80]
	ldr x17, [x16,#96]
	mov sp, x17
	ret
