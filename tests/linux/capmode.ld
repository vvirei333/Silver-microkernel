/* Capsicum-lite probe: one RX segment, no writable globals. */
ENTRY(_start)
SECTIONS
{
  . = 0x400000;
  .text : { *(.text*) *(.rodata*) *(.data*) }
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
