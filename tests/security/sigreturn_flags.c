/* Security regression: a signal frame may carry user PSTATE bits, but only
 * the safe ones.
 *
 *   linuxrun /sfs/sec-sigreturn-flags
 *
 * The handler sets NZCV.Z (bit 30 — a legitimate condition flag), plus SS
 * (21), IL (20) and a reserved bit (23). The kernel must preserve NZCV, strip
 * SS/IL/reserved, and resume at EL0. If it rejects bit 30 as if it were the
 * AArch32 selector, or restores SS/IL, this exits without the marker.
 *
 * Expected: "sigreturn flags ok" and exit 0.
 */
static long sys6(long n, long a, long b, long c, long d, long e, long f) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    register long x4 __asm__("x4") = e;
    register long x5 __asm__("x5") = f;
    __asm__ volatile("svc #0"
                     : "+r"(x0)
                     : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5)
                     : "memory");
    return x0;
}

struct sigaction {
    void *handler;
    unsigned long flags;
    void *restorer;
    unsigned long mask;
};

#define UCONTEXT_PSTATE 440

static volatile long handler_ran;

static void handler(int signal, void *info, void *ucontext) {
    (void)signal;
    (void)info;
    unsigned long *pstate = (unsigned long *)((unsigned char *)ucontext +
                                              UCONTEXT_PSTATE);
    /* NZCV.Z (30) is preserved; SS (21), IL (20) and bit 23 are stripped. */
    *pstate = (1ul << 30) | (1ul << 21) | (1ul << 20) | (1ul << 23);
    handler_ran = 1;
}

void _start(void) {
    struct sigaction action = { 0 };
    action.handler = (void *)handler;
    action.flags = 4; /* SA_SIGINFO */
    sys6(134 /* rt_sigaction */, 10 /* SIGUSR1 */, (long)&action, 0, 8, 0, 0);
    long pid = sys6(172 /* getpid */, 0, 0, 0, 0, 0, 0);
    sys6(129 /* kill */, pid, 10, 0, 0, 0, 0);
    if (!handler_ran) {
        sys6(64, 1, (long)"sigreturn flags: handler did not run\n", 40, 0, 0, 0);
        sys6(93, 1, 0, 0, 0, 0, 0);
    }
    sys6(64, 1, (long)"sigreturn flags ok\n", 20, 0, 0, 0);
    sys6(93, 0, 0, 0, 0, 0, 0);
    for (;;) {
    }
}
