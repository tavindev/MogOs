ENTRY(_start)

/* As crates/user/link.ld: one RX and one RW PT_LOAD from 4 GiB, which the kernel's ELF parser requires. */
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
    .init_array : { *(SORT_BY_INIT_PRIORITY(.init_array.*) .init_array) } :data
    .fini_array : { *(SORT_BY_INIT_PRIORITY(.fini_array.*) .fini_array) } :data
    .data : { *(.data .data.*) } :data
    .got : { *(.got .got.plt) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
