// MogOs: soft-float, so no d8-d15 to save; the jmp_buf layout stays musl's.
.global __setjmp
.global _setjmp
.global setjmp
.type __setjmp,@function
.type _setjmp,@function
.type setjmp,@function
__setjmp:
_setjmp:
setjmp:
	stp x19, x20, [x0,#0]
	stp x21, x22, [x0,#16]
	stp x23, x24, [x0,#32]
	stp x25, x26, [x0,#48]
	stp x27, x28, [x0,#64]
	stp x29, x30, [x0,#80]
	mov x2, sp
	str x2, [x0,#104]
	mov x0, #0
	ret
