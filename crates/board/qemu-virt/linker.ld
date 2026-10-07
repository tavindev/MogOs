ENTRY(_start)

SECTIONS
{
    /* QEMU only loads its 1 MiB DTB at RAM base if it fits below the image. The image's 2 MiB is mapped by 4 KiB pages
       (`arch::KernelMap`), so every boundary below is page aligned. */
    . = 0x40200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    __text_end = .;
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(4096);
    __rodata_end = .;
    .data : { *(.data .data.*) }

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

    /* Core 0's 64 KiB boot stack, with an unmapped 4 KiB guard page below it (the other cores' stacks are in their
       per-CPU blocks, allocated at boot). */
    . = ALIGN(4096);
    __boot_guard = .;
    . += 0x1000 + 0x10000;
    __stack_top = .;
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}

ASSERT(__kernel_start % 0x200000 == 0, "the image must start a 2 MiB block")
ASSERT(__kernel_end <= __kernel_start + 0x200000, "the image must fit its 2 MiB block, which alone is mapped by pages")
