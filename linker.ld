ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    /* Common AArch64 DRAM load address; replace with the firmware address later. */
    . = 0x40080000;
    __kernel_start = .;
    __text_start = .;

    .text : ALIGN(4096)
    {
        KEEP(*(.text.boot))
        KEEP(*(.text.vectors))
        *(.text .text.*)
    } :text
    __text_end = .;

    .rodata : ALIGN(4096)
    {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(4096)
    {
        __data_start = .;
        *(.data .data.*)
    } :data

    .bss (NOLOAD) : ALIGN(4096)
    {
        __bss_start = .;
        *(.bss .bss.* COMMON)
        __bss_end = .;
    } :data

    .stack (NOLOAD) : ALIGN(16)
    {
        __stack_bottom = .;
        . += 16K;
        __stack_top = .;
    } :data

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ :
    {
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
    }
}
