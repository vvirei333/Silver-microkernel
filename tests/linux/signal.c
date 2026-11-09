/* Signals: rt_sigaction, rt_sigprocmask, kill/tgkill, SIGCHLD on child exit.
 *
 *   linuxrun /sfs/linux-signal
 *
 * Expected: "signal ok" and exit 0.
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

struct sigaction {
    unsigned long handler;
    unsigned long flags;
    unsigned long restorer;
    unsigned long mask;
};

struct timespec {
    long seconds;
    long nanoseconds;
};

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

void _start(void) {
    /* Ignore SIGCHLD explicitly and read the old disposition back. */
    struct sigaction ignore = {1 /* SIG_IGN */, 0, 0, 0};
    struct sigaction previous = {0, 0, 0, 0};
    if (sys4(134, 17 /* SIGCHLD */, (long)&ignore, (long)&previous, 8) != 0) {
        fail("sigaction failed\n", 16);
    }
    if (previous.handler != 0) {
        fail("sigaction old wrong\n", 19);
    }

    /* Block and unblock SIGUSR1 (10); the old mask must round-trip. */
    unsigned long set = 1UL << 9;
    unsigned long old = 0;
    if (sys4(135, 0 /* SIG_BLOCK */, (long)&set, (long)&old, 8) != 0 || old != 0) {
        fail("sigprocmask block failed\n", 24);
    }
    if (sys4(135, 1 /* SIG_UNBLOCK */, (long)&set, (long)&old, 8) != 0 ||
        old != set) {
        fail("sigprocmask unblock failed\n", 26);
    }

    /* Normal child: exit status 0 and SIGCHLD pending is cleared by wait4. */
    long pid = sys5(220, 17, 0, 0, 0, 0);
    if (pid == 0) {
        sys3(93, 0, 0, 0);
        for (;;) {
        }
    }
    int status = 0;
    if (sys4(260, pid, (long)&status, 0, 0) != pid || ((status >> 8) & 0xff) != 0) {
        fail("wait failed\n", 12);
    }

    /* SIGKILL terminates a sleeping child; wait4 must still reap it. */
    pid = sys5(220, 17, 0, 0, 0, 0);
    if (pid == 0) {
        struct timespec sleep = {5, 0};
        sys2(101, (long)&sleep, 0); /* nanosleep */
        sys3(93, 9, 0, 0);
        for (;;) {
        }
    }
    if (sys2(129, pid, 9 /* SIGKILL */) != 0) {
        fail("kill failed\n", 12);
    }
    if (sys4(260, pid, (long)&status, 0, 0) != pid) {
        fail("kill wait failed\n", 17);
    }

    sys3(64, 1, (long)"signal ok\n", 10);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
