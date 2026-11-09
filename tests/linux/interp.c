/* Minimal PT_INTERP interpreter for the ET_DYN test.
 *
 * Reads the initial stack (argc/argv/envp/auxv), finds AT_ENTRY and jumps to
 * the main program. It uses no relocations and no libc, so a tiny ET_EXEC
 * loaded at INTERP_BIAS is enough to prove the loader mechanics.
 *
 * Built with -nostdlib -static -Wl,-T,tests/linux/chain.ld
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
    unsigned long *sp;
    __asm__ volatile("mov %0, sp" : "=r"(sp));
    unsigned long argc = *sp;
    sp += 1 + argc + 1;      /* argc, argv.., NULL */
    while (*sp) {
        sp++;                /* environment strings */
    }
    sp++;                    /* NULL after envp */
    unsigned long entry = 0;
    while (sp[0] != 0) {     /* auxv pairs */
        if (sp[0] == 9) {    /* AT_ENTRY */
            entry = sp[1];
        }
        sp += 2;
    }
    if (entry == 0) {
        sys3(64, 1, (long)"no AT_ENTRY\n", 12);
        sys3(93, 1, 0, 0);
    }
    __asm__ volatile("br %0" ::"r"(entry));
    for (;;) {
    }
}
