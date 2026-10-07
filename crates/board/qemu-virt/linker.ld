ENTRY(_start)

SECTIONS
{
    /* QEMU only loads its 1 MiB DTB at RAM base if it fits below the image. */
    . = 0x40200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(8) { *(.rodata .rodata.*) }
    .data   : ALIGN(8) { *(.data .data.*) }

    /* The per-CPU template: each core's area is a copy, made at boot; the template itself is never written. */
    .percpu : ALIGN(64) {
        __percpu_start = .;
        KEEP(*(.percpu .percpu.*))
        . = ALIGN(16);
        __percpu_end = .;
    }

    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    . = ALIGN(16);
    . += 0x10000;
    __stack_top = .;
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
