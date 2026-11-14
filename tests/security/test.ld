/* Security tests are freestanding Linux ABI programs; one RX segment plus
   data/bss is enough (same layout as the tests/linux probes). */
ENTRY(_start)
SECTIONS
{
  . = 0x400000;
  .text : { *(.text*) *(.rodata*) }
  . = ALIGN(0x1000);
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
}
