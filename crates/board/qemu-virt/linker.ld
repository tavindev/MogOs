ENTRY(_start)

SECTIONS
{
    /* QEMU only loads its 1 MiB DTB at RAM base if it fits below the image. Text, read-only data and the rest each
       start a 2 MiB block, mapped whole (`arch::KernelMap`): no 4 KiB page of the image takes a TLB entry. */
    . = 0x40200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    . = ALIGN(0x200000);
    __text_end = .;
    /* `.data.rel.ro` holds link-time constants: no relocation writes it at run time. */
    .rodata : { *(.rodata .rodata.*) *(.data.rel.ro .data.rel.ro.*) }

    . = ALIGN(0x200000);
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

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}

/* Core 0's 64 KiB boot stack lives below the image, at the top of RAM's first 2 MiB (mapped by 4 KiB pages, the DTB
   below it), with an unmapped 4 KiB guard page below it; the other cores' stacks are in their per-CPU blocks. */
__stack_top = __kernel_start;
__boot_guard = __stack_top - 0x11000;
__stacks = __boot_guard;

ASSERT(__kernel_start % 0x200000 == 0, "the image must start a 2 MiB block")
