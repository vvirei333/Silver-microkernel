/* User-installed signal handlers: rt_sigframe + rt_sigreturn.
 *
 *   linuxrun /sfs/linux-sighandler
 *
 * Expected: "sigframe ok" and exit 0.
 *
 * Installs a SA_SIGINFO handler for SIGUSR1, raises it with kill(getpid()),
 * checks the siginfo fields, then blocks SIGUSR2, raises it (must stay
 * pending), unblocks it and checks the handler runs before the syscall
 * returns.
 */
static long sys5(long n, long a, long b, long c, long d, long e) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    register long x4 __asm__("x4") = e;
    __asm__ volatile("svc #0"
                     : "+r"(x0)
                     : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4)
                     : "memory");
    return x0;
}

static long sys4(long n, long a, long b, long c, long d) {
    return sys5(n, a, b, c, d, 0);
}

static long sys3(long n, long a, long b, long c) {
    return sys5(n, a, b, c, 0, 0);
}

static long sys2(long n, long a, long b) { return sys5(n, a, b, 0, 0, 0); }

static long sys1(long n, long a) { return sys5(n, a, 0, 0, 0, 0); }

struct kernel_sigaction {
    unsigned long handler;
    unsigned long flags;
    unsigned long restorer;
    unsigned long mask;
};

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

static void restorer(void) {
    register long x8 __asm__("x8") = 139; /* rt_sigreturn */
    __asm__ volatile("svc #0" : : "r"(x8) : "memory");
    for (;;) {
    }
}

static volatile unsigned long usr1_seen;
static volatile unsigned long usr1_signo;
static volatile unsigned long usr1_code;
static volatile unsigned long usr2_seen;
static volatile unsigned long stack_marker = 0x5A5A5A5A;

static void handler_info(int signal, void *info, void *ucontext) {
    (void)ucontext;
    const unsigned char *bytes = (const unsigned char *)info;
    usr1_seen = (unsigned long)signal;
    usr1_signo = (unsigned long)bytes[0] | ((unsigned long)bytes[1] << 8) |
                 ((unsigned long)bytes[2] << 16) | ((unsigned long)bytes[3] << 24);
    usr1_code = (unsigned long)bytes[8] | ((unsigned long)bytes[9] << 8) |
                ((unsigned long)bytes[10] << 16) | ((unsigned long)bytes[11] << 24);
    stack_marker = 0xA5A5A5A5;
}

static void handler_usr2(int signal) {
    usr2_seen = (unsigned long)signal;
}

void _start(void) {
    const long SIGUSR1 = 10;
    const long SIGUSR2 = 12;
    const long SIG_BLOCK = 0;
    const long SIG_UNBLOCK = 1;
    const unsigned long SA_SIGINFO = 4;
    const unsigned long SA_RESTORER = 0x04000000;

    unsigned long pid = (unsigned long)sys1(172, 0); /* getpid */

    struct kernel_sigaction action = {
        (unsigned long)handler_info, SA_SIGINFO | SA_RESTORER,
        (unsigned long)restorer, 0};
    if (sys4(134, SIGUSR1, (long)&action, 0, 8) != 0) {
        fail("sigaction failed\n", 18);
    }

    if (sys2(129, (long)pid, SIGUSR1) != 0) {
        fail("kill usr1 failed\n", 17);
    }
    if (usr1_seen != (unsigned long)SIGUSR1) {
        fail("usr1 handler did not run\n", 26);
    }
    if (usr1_signo != (unsigned long)SIGUSR1 || usr1_code != 0) {
        fail("siginfo wrong\n", 14);
    }
    if (stack_marker != 0xA5A5A5A5) {
        fail("rt_sigreturn did not restore\n", 30);
    }

    struct kernel_sigaction usr2 = {
        (unsigned long)handler_usr2, SA_RESTORER, (unsigned long)restorer, 0};
    if (sys4(134, SIGUSR2, (long)&usr2, 0, 8) != 0) {
        fail("sigaction usr2 failed\n", 23);
    }
    unsigned long set = 1UL << (SIGUSR2 - 1);
    unsigned long old = 0;
    if (sys4(135, SIG_BLOCK, (long)&set, (long)&old, 8) != 0) {
        fail("sigprocmask block failed\n", 26);
    }
    if (sys2(129, (long)pid, SIGUSR2) != 0) {
        fail("kill usr2 failed\n", 17);
    }
    if (usr2_seen != 0) {
        fail("blocked signal was delivered\n", 30);
    }
    if (sys4(135, SIG_UNBLOCK, (long)&set, 0, 8) != 0) {
        fail("sigprocmask unblock failed\n", 28);
    }
    if (usr2_seen != (unsigned long)SIGUSR2) {
        fail("pending signal not delivered\n", 29);
    }

    sys3(64, 1, (long)"sigframe ok\n", 12);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
