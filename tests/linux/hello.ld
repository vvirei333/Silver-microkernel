/* Foreign test program linked at the address a normal Linux binary uses.
 *
 * `load_linux_elf` honours `p_vaddr`, so this no longer has to live in the
 * native image window. Everything is one RX segment: the test writes no
 * globals, and the loader rejects W+X segments.
 *
 * The kernel stack is built by the loader; `_start` never returns.
 */
ENTRY(_start)
SECTIONS
{
  . = 0x400000;
  .text : { *(.text*) *(.rodata*) *(.data*) }
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
