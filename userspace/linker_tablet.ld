ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    . = 0x80000000;

    .text : ALIGN(4096)
    {
        *(.text._start)
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4096)
    {
        *(.rodata .rodata.*)
        /* litehtml's charset prescan throws a private exception; keep the
           unwind tables that go with it. */
        *(.eh_frame .eh_frame.*)
        *(.gcc_except_table .gcc_except_table.*)
    } :rodata

    .data : ALIGN(4096)
    {
        *(.data .data.*)
        *(.got .got.*)
        /* C++ static constructors (litehtml string ids, libstdc++ iostream
           init). Newlib's __libc_init_array walks these. */
        . = ALIGN(8);
        __preinit_array_start = .;
        KEEP(*(.preinit_array))
        __preinit_array_end = .;
        __init_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array))
        __init_array_end = .;
        __fini_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*)))
        KEEP(*(.fini_array))
        __fini_array_end = .;
    } :data

    .bss (NOLOAD) : ALIGN(4096)
    {
        *(.bss .bss.* COMMON)
    } :data

    /DISCARD/ :
    {
        *(.comment*)
        *(.note*)
    }
}
