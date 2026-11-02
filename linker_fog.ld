ENTRY(__boot_entry)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    /* High-half VMA, low physical LMA. The Redmi 10C (SM6225 "khaje") is a
       4 GiB-base Qualcomm platform, so DRAM starts at 0x4000_0000. The link
       address 0x4008_0000 is the conventional ABL kernel slot on this family
       (base + 0x80000); it is a CONFIGURED placement, not a hardware fact
       measured on this unit. The default fog build records the address ABL
       actually used in BOOT_PARAMS and parks on a mismatch; only the explicit
       `fog-reloc` feature copies the image here. Address match alone is not
       proof that the live memory map is safe. */
    __kernel_virt_base = 0xFFFF000000000000;
    __dram_base = 0x40000000;
    __kernel_link_pa = 0x40080000;
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
        . += 32K;
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
