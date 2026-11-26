/* Group-kill probe: one RX segment and a tiny data/bss tail. */
ENTRY(_start)
SECTIONS
{
  . = 0x400000;
  .text : { *(.text*) *(.rodata*) }
  . = ALIGN(0x1000);
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
}
