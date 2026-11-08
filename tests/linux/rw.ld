/* Two real PT_LOAD segments at 0x400000: text RX, then data RW. The gap and
 * the separate permission classes are what the Linux loader has to honour.
 */
ENTRY(_start)
SECTIONS
{
  . = 0x400000;
  .text : { *(.text*) *(.rodata*) }
  . = ALIGN(0x1000);
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
}
