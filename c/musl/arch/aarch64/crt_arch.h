/* MogOs starts a process with x0 = string count, x1 = their address, x2 = their length (no Linux stack layout);
   __mog_start maps a real stack, builds argc, argv, envp and auxv on it and returns it. */
__asm__(
".text \n"
".global " START "\n"
".type " START ",%function\n"
START ":\n"
"	mov x29, #0\n"
"	mov x30, #0\n"
"	bl __mog_start\n"
"	mov sp, x0\n"
"	b " START "_c\n"
);
