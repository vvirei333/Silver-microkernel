/* Security regression: a signal frame must not select AArch32.
 *
 *   linuxrun /sfs/sec-sigreturn-aa32
 *
 * PSTATE.M[4] = 1 (0x10, AArch32 USR) is a different execution state; this
 * kernel has no AArch32 personality, so an eret to it would land in an
 * unsupported mode. The kernel must reject the frame and kill the task with
 * SIGSEGV, like any other non-EL0t PSTATE.
 *
 * Expected: "sigreturn aa32 blocked" and exit 0.
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

static void handler(int signal, void *info, void *ucontext) {
    (void)signal;
    (void)info;
    unsigned long *pstate = (unsigned long *)((unsigned char *)ucontext +
                                              UCONTEXT_PSTATE);
    *pstate = 0x10; /* M[4] = 1: AArch32 USR */
}

static void child(void) {
    struct sigaction action = { 0 };
    action.handler = (void *)handler;
    action.flags = 4; /* SA_SIGINFO */
    sys6(134 /* rt_sigaction */, 10 /* SIGUSR1 */, (long)&action, 0, 8, 0, 0);
    long pid = sys6(172 /* getpid */, 0, 0, 0, 0, 0, 0);
    sys6(129 /* kill */, pid, 10, 0, 0, 0, 0);
    sys6(93, 99, 0, 0, 0, 0, 0);
    for (;;) {
    }
}

void _start(void) {
    long pid = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (pid == 0) {
        child();
    }
    int status = 0;
    if (sys6(260 /* wait4 */, pid, (long)&status, 0, 0, 0, 0) != pid) {
        sys6(64, 1, (long)"sigreturn aa32: wait failed\n", 30, 0, 0, 0);
        sys6(93, 1, 0, 0, 0, 0, 0);
    }
    if ((status & 0x7f) == 11 || ((status >> 8) & 0xff) == 139) {
        sys6(64, 1, (long)"sigreturn aa32 blocked\n", 25, 0, 0, 0);
        sys6(93, 0, 0, 0, 0, 0, 0);
    }
    sys6(64, 1, (long)"sigreturn aa32: unexpected status\n", 36, 0, 0, 0);
    sys6(93, 1, 0, 0, 0, 0, 0);
    for (;;) {
    }
}
