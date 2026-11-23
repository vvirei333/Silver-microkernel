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
       addresses and starts at __boot_entry with the MMU still disabled.

       The three numbers below default to QEMU `virt` but can be overridden per
       board without editing this file: build with

           SILVER_LINK_PA=0x200000 SILVER_DRAM_BASE=0x0 cargo kernel

       and `build.rs` passes `--defsym=__silver_link_pa=...`, which the
       DEFINED() guards below pick up. A board is a build parameter, not a
       patched linker script. */
    __kernel_virt_base = DEFINED(__silver_kernel_virt_base) ? __silver_kernel_virt_base : 0xFFFF000000000000;
    __dram_base = DEFINED(__silver_dram_base) ? __silver_dram_base : 0x40000000;
    __kernel_link_pa = DEFINED(__silver_link_pa) ? __silver_link_pa : 0x40080000;
    . = __kernel_virt_base + __kernel_link_pa;
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
        . += 64K;
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
