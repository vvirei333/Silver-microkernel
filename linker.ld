ENTRY(__boot_entry)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    /* High-half VMA, low physical LMA. QEMU loads the ELF at the physical
       addresses and starts at __boot_entry with the MMU still disabled. */
    __kernel_virt_base = 0xFFFF000000000000;
    . = 0xFFFF000040080000;
    __kernel_start = .;
    __text_start = .;

    .text : AT(ADDR(.text) - __kernel_virt_base) ALIGN(4096)
    {
        KEEP(*(.text.boot))
        KEEP(*(.text.vectors))
        *(.text .text.*)
    } :text
    __text_end = .;

    .rodata : AT(ADDR(.rodata) - __kernel_virt_base) ALIGN(4096)
    {
        *(.rodata .rodata.*)
    } :rodata

    .data : AT(ADDR(.data) - __kernel_virt_base) ALIGN(4096)
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

    /* ELF e_entry must be the physical boot address, not the high VMA. */
    __boot_entry = _start - __kernel_virt_base;

    /DISCARD/ :
    {
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
    }
}
