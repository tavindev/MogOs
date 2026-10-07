ENTRY(_start)

/* The kernel maps each PT_LOAD at its page-aligned address, below a guard page and the two stack pages that end at
   4 GiB + 2 MiB. */
PHDRS
{
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x100000000;
    .text : { *(.text .text.*) } :text
    .rodata : { *(.rodata .rodata.*) } :text

    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
