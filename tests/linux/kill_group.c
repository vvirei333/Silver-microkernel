/* SIGKILL to a process leader must kill the whole thread group.
 *
 *   linuxrun /sfs/linux-kill-group
 *
 * A child process starts a worker that sleeps, then touches a private page.
 * The parent, outside the group, sends SIGKILL to the child leader. If only
 * the leader dies, the worker wakes into a freed address space and faults; if
 * the kernel retires the group, wait4 returns a WIFSIGNALED status with
 * WTERMSIG == SIGKILL and no fault appears.
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

static void print(const char *text, long length) {
    sys3(64, 1, (long)text, length);
}

static void delay(long seconds) {
    struct timespec {
        long seconds;
        long nanoseconds;
    } request = {seconds, 0};
    sys6(101, (long)&request, 0, 0, 0, 0, 0);
}

static volatile char *page;
static int self_signal;

static void worker(void) {
    delay(2);
    if (self_signal) {
        /* A worker's fatal signal must terminate its whole group and leave a
         * WTERMSIG status on the leader record. */
        sys3(129, 0 /* own process */, 15 /* SIGTERM */, 0);
        for (;;) {
        }
    }
    page[0] = 'x';
    page[1] = 'y';
    print("worker should have been killed\n", 30);
    sys3(93, 1, 0, 0);
    for (;;) {
    }
}

static void child(void) {
    const long SIZE = 64 * 1024;
    const unsigned long CLONE_VM = 0x00000100;
    const unsigned long CLONE_SIGHAND = 0x00000800;
    const unsigned long CLONE_THREAD = 0x00010000;
    const unsigned long CLONE_SETTLS = 0x00080000;
    const unsigned long CLONE_CHILD_SETTID = 0x01000000;
    const unsigned long CLONE_CHILD_CLEARTID = 0x00200000;

    unsigned char *stack = (unsigned char *)sys6(
        222, 0, SIZE, 3, 0x22, -1, 0);
    page = (char *)sys6(222, 0, 4096, 3, 0x22, -1, 0);
    if ((long)stack < 0 || (long)page < 0) {
        print("child mmap failed\n", 17);
        sys3(93, 2, 0, 0);
    }
    unsigned long stack_top = ((unsigned long)stack + SIZE) & ~15UL;
    static unsigned long tls_block[8];
    static volatile int child_tid_word;
    long tid = sys6(220,
                    CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS |
                        CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID,
                    (long)stack_top, 0, (long)tls_block,
                    (long)&child_tid_word, 0);
    if (tid < 0) {
        print("child clone failed\n", 19);
        sys3(93, 2, 0, 0);
    }
    if (tid == 0) {
        worker();
    }
    /* Keep the leader alive and runnable while the parent kills it. */
    for (;;) {
        page[2] = 1;
    }
}

void _start(void) {
    long pid = sys6(220, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (pid == 0) {
        child();
    }
    if (pid < 0) {
        print("fork failed\n", 12);
        sys3(93, 2, 0, 0);
    }
    delay(1);
    sys3(129, pid, 9 /* SIGKILL */, 0);
    int status = 0;
    long waited = sys6(260, pid, (long)&status, 0, 0, 0, 0);
    if (waited != pid) {
        print("wait failed\n", 12);
        sys3(93, 3, 0, 0);
    }
    /* Let a leaked worker wake and fault before declaring success. */
    delay(3);
    if ((status & 0x7f) != 9) {
        print("group kill wrong status\n", 24);
        sys3(93, 4, 0, 0);
    }

    /* The same group exit initiated by a worker must carry its signal. */
    self_signal = 1;
    pid = sys6(220, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (pid == 0) {
        child();
    }
    if (pid < 0) {
        sys3(93, 5, 0, 0);
    }
    if (sys6(260, pid, (long)&status, 0, 0, 0, 0) != pid ||
        (status & 0x7f) != 15) {
        print("group signal status wrong\n", 26);
        sys3(93, 6, 0, 0);
    }
    print("group kill signal ok\n", 21);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
