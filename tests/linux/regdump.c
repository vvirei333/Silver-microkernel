/* Fault with known registers: the kernel dump must report these values.
 *
 *   linuxrun /sfs/regdump
 *
 * x0 is NULL when the load faults; x19/x20/x8 carry sentinels. If the dump
 * shows anything else, the saved EL0 bank is not the faulting context.
 */
void _start(void) {
    __asm__ volatile(
        "mov x19, #0x1234\n"
        "mov x20, #0x5678\n"
        "mov x8,  #0x99\n"
        "mov x0,  #0\n"
        "ldr x1,  [x0]\n"
        "brk #0\n");
    for (;;) {
    }
}
