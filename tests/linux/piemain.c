/* ET_DYN/PIE main program with a custom PT_INTERP.
 *
 *   linuxrun /sfs/linux-pie
 *
 * Expected: "pie main", exit status 7. Built with:
 *   -nostdlib -fPIE -pie -Wl,--dynamic-linker=/sfs/ld-test
 * The kernel loads it at MAIN_BIAS (0x1000_0000), loads /sfs/ld-test as the
 * interpreter at INTERP_BIAS (0x2000_0000), and starts at the interpreter.
 */
static long sys3(long n, long a, long b, long c) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}

void _start(void) {
    sys3(64, 1, (long)"pie main\n", 9);
    sys3(93, 7, 0, 0);
    for (;;) {
    }
}
