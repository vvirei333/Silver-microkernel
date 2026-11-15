/* Security regression: SIGKILL cannot be blocked (L2).
 *
 *   linuxrun /sfs/sec-blocked-kill
 *
 * The child masks every signal (the kernel must strip SIGKILL and SIGSTOP)
 * and then sleeps for four seconds. The parent sends SIGKILL and polls
 * wait4(WNOHANG) for up to five seconds. On the fixed kernel the child is
 * gone almost immediately and its status is the kernel's killed status (1);
 * if SIGKILL were blockable the pending bit would never be delivered and the
 * child would exit normally with 42, which the parent reports as a failure.
 * The child's own deadline means the probe fails fast instead of hanging the
 * shell even on the broken kernel.
 *
 * Expected: "blocked kill ok" and exit 0.
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

static long sys3(long n, long a, long b, long c) {
    return sys6(n, a, b, c, 0, 0, 0);
}

struct timespec {
    long seconds;
    long nanoseconds;
};

static __attribute__((noreturn)) void fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    for (;;) {
    }
}

static void sleep_milliseconds(long milliseconds) {
    struct timespec request = { milliseconds / 1000, (milliseconds % 1000) * 1000000 };
    sys6(101 /* nanosleep */, (long)&request, 0, 0, 0, 0, 0);
}

static void child(void) {
    /* A superset of the 64-bit signal set: SIGKILL (bit 8) and SIGSTOP
       (bit 18) are in here and must be stripped by set_blocked. */
    unsigned long all = ~0ul;
    sys6(135 /* rt_sigprocmask */, 2 /* SIG_SETMASK */, (long)&all, 0, 8, 0, 0);

    /* Read the mask back. SIGKILL/SIGSTOP must not be in it: a mask that
       still contains them is the L2 bug, even though linux_kill happens to
       short-circuit SIGKILL today. Exiting 77 before the parent's kill makes
       the regression fail deterministically instead of relying on delivery. */
    unsigned long current = 0;
    sys6(135, 0 /* how is ignored when set is NULL */, 0, (long)&current, 8, 0, 0);
    if (current & ((1ul << 8) | (1ul << 18))) {
        sys3(93 /* exit */, 77, 0, 0);
    }

    for (int index = 0; index < 4; index++) {
        sleep_milliseconds(1000);
    }
    sys3(93 /* exit */, 42, 0, 0);
    for (;;) {
    }
}

void _start(void) {
    long child_pid = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (child_pid == 0) {
        child();
    }
    if (child_pid < 0) {
        fail("blocked kill: clone failed\n", 26);
    }

    /* Let the child reach rt_sigprocmask and the sleep loop before the kill. */
    sleep_milliseconds(100);

    if (sys6(129 /* kill */, child_pid, 9 /* SIGKILL */, 0, 0, 0, 0) < 0) {
        fail("blocked kill: kill failed\n", 25);
    }

    int status = 0;
    long reaped = 0;
    for (int attempt = 0; attempt < 50; attempt++) {
        sleep_milliseconds(100);
        reaped = sys6(260 /* wait4 */, child_pid, (long)&status, 1 /* WNOHANG */, 0, 0, 0);
        if (reaped == child_pid || reaped < 0) {
            break;
        }
    }

    if (reaped != child_pid) {
        fail("blocked kill: child was not reaped\n", 33);
    }
    /* The kernel reports a killed task as status 1; the child's normal exit
       would be 42 (or anything else), which proves SIGKILL was blocked. */
    if ((status >> 8) == 1) {
        sys3(64, 1, (long)"blocked kill ok\n", 16);
        sys3(93, 0, 0, 0);
    }
    fail("blocked kill: child survived SIGKILL\n", 35);
    for (;;) {
    }
}
