/* Asynchronous signals and sigaltstack.
 *
 *   linuxrun /sfs/linux-asyncsig
 *
 * Expected: "async sig ok" and exit 0.
 *
 * The parent installs a SIGUSR1 handler, forks a child, and busy-waits in EL0
 * (no syscall) so the handler can only be entered from the timer-IRQ return
 * path after the child sends the signal. A second handler runs on a
 * registered alternate stack (SA_ONSTACK) and checks that SP is inside it.
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

struct stack_t {
    unsigned long sp;
    int flags;
    int pad;
    unsigned long size;
};

struct timespec {
    long seconds;
    long nanoseconds;
};

static void restorer(void) {
    register long x8 __asm__("x8") = 139;
    __asm__ volatile("svc #0" : : "r"(x8) : "memory");
    for (;;) {
    }
}

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

static volatile unsigned long async_seen;
static volatile unsigned long alt_seen;
static volatile unsigned long alt_on_stack;
static unsigned char alt_memory[8192] __attribute__((aligned(16)));

static void handler_async(int signal) {
    async_seen = (unsigned long)signal;
}

static void handler_alt(int signal) {
    unsigned long sp;
    __asm__ volatile("mov %0, sp" : "=r"(sp));
    alt_seen = (unsigned long)signal;
    alt_on_stack = (sp >= (unsigned long)alt_memory &&
                    sp < (unsigned long)alt_memory + sizeof(alt_memory));
}

void _start(void) {
    const long SIGUSR1 = 10;
    const long SIGUSR2 = 12;
    const unsigned long SA_RESTORER = 0x04000000;
    const unsigned long SA_ONSTACK = 0x08000000;

    struct kernel_sigaction action = {
        (unsigned long)handler_async, SA_RESTORER, (unsigned long)restorer, 0};
    if (sys4(134, SIGUSR1, (long)&action, 0, 8) != 0) {
        fail("sigaction async failed\n", 24);
    }
    struct kernel_sigaction alt_action = {
        (unsigned long)handler_alt, SA_RESTORER | SA_ONSTACK,
        (unsigned long)restorer, 0};
    if (sys4(134, SIGUSR2, (long)&alt_action, 0, 8) != 0) {
        fail("sigaction alt failed\n", 22);
    }
    struct stack_t alt = {
        (unsigned long)alt_memory, 0, 0, sizeof(alt_memory)};
    if (sys2(132, (long)&alt, 0) != 0) {
        fail("sigaltstack failed\n", 20);
    }

    long pid = sys5(220, 17, 0, 0, 0, 0); /* clone(SIGCHLD) */
    if (pid < 0) {
        fail("fork failed\n", 12);
    }
    if (pid == 0) {
        struct timespec delay = {0, 100 * 1000 * 1000};
        sys3(101, (long)&delay, 0, 0); /* nanosleep */
        sys2(129, (long)sys1(173, 0), SIGUSR1); /* kill(getppid()) */
        sys3(93, 0, 0, 0);
        for (;;) {
        }
    }

    /* No syscall in this loop: the handler can only arrive on a timer IRQ. */
    for (volatile unsigned long spin = 0; spin < 80000000UL; spin++) {
        if (async_seen != 0) {
            break;
        }
    }
    if (async_seen != (unsigned long)SIGUSR1) {
        fail("async signal not delivered\n", 27);
    }

    /* Synchronous raise of the SA_ONSTACK handler. */
    if (sys2(129, (long)sys1(172, 0), SIGUSR2) != 0) {
        fail("kill alt failed\n", 15);
    }
    if (alt_seen != (unsigned long)SIGUSR2 || !alt_on_stack) {
        fail("altstack handler wrong\n", 22);
    }

    int status = 0;
    if (sys4(260, pid, (long)&status, 0, 0) != pid || ((status >> 8) & 0xff) != 0) {
        fail("child wait failed\n", 18);
    }

    sys3(64, 1, (long)"async sig ok\n", 13);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
