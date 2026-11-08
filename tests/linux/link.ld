/* The Linux ABI slice 1 loads foreign images through the native image window
 * (0x4000_0000), because the loader still maps one contiguous run at
 * USER_IMAGE_VA. A real distro binary links at 0x40_0000; supporting that is
 * the next slice (arbitrary p_vaddr mappings).
 *
 * Everything lives in one RX segment: the test writes no globals, and the
 * kernel loader rejects holes and W+X segments.
 */
ENTRY(_start)
SECTIONS
{
  . = 0x40000000;
  .text : { *(.text*) *(.rodata*) *(.data*) }
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
