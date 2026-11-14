/* Security regression: rt_sigreturn must not let a task choose its EL.
 *
 *   linuxrun /sfs/sec-sigreturn-pstate
 *
 * The kernel copies the saved `pstate` from the user's ucontext back into the
 * SVC frame and the trampoline writes it to SPSR_EL1 before `eret`. If that
 * value is not validated to be EL0t, a task can return from a signal handler
 * with PSTATE.M = EL1h and execute its own code at EL1.
 *
 * Child: installs a SIGUSR1 handler, has it set pstate = 0x5 (EL1h), raises
 * the signal, and places `udf #0` right after the `kill` syscall. With the
 * bug the undefined instruction traps from EL1 and the kernel panics; with
 * the fix the child is killed with SIGSEGV before it ever returns.
 *
 * Expected: "sigreturn pstate blocked" and exit 0 (child died with SIGSEGV).
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

static void print(const char *text, long length) {
    sys6(64, 1, (long)text, length, 0, 0, 0);
}

static void exit_with(long status) {
    sys6(93, status, 0, 0, 0, 0, 0);
    for (;;) {
    }
}

struct sigaction {
    void *handler;
    unsigned long flags;
    void *restorer;
    unsigned long mask;
};

/* The handler's third argument points at the ucontext. Layout (linux_signal.rs):
   uc_mcontext at +168, fault_address at +176, x0-x30 at +184, sp at +424,
   pc at +432, pstate at +440. */
#define UCONTEXT_PSTATE 440

static void handler(int signal, void *info, void *ucontext) {
    (void)signal;
    (void)info;
    unsigned long *pstate = (unsigned long *)((unsigned char *)ucontext +
                                              UCONTEXT_PSTATE);
    *pstate = 0x5; /* PSTATE.M = EL1h */
}

static void child(void) {
    struct sigaction action = { 0 };
    action.handler = (void *)handler;
    action.flags = 4; /* SA_SIGINFO */
    sys6(134 /* rt_sigaction */, 10 /* SIGUSR1 */, (long)&action, 0, 8, 0, 0);
    long pid = sys6(172 /* getpid */, 0, 0, 0, 0, 0, 0);
    sys6(129 /* kill */, pid, 10, 0, 0, 0, 0);
    /* If the kernel accepted EL1h, this executes at EL1. */
    __asm__ volatile("udf #0");
    exit_with(99);
}

void _start(void) {
    long pid = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (pid == 0) {
        child();
    }
    int status = 0;
    if (sys6(260 /* wait4 */, pid, (long)&status, 0, 0, 0, 0) != pid) {
        print("sigreturn pstate: wait failed\n", 30);
        exit_with(1);
    }
    /* The kernel kills a bad sigreturn with SIGSEGV; this ABI reports it as
       exit status 128+signal (139) rather than a wait-status signal. */
    if ((status & 0x7f) == 11 || ((status >> 8) & 0xff) == 139) {
        print("sigreturn pstate blocked\n", 25);
        exit_with(0);
    }
    print("sigreturn pstate: unexpected status\n", 36);
    exit_with(1);
}
