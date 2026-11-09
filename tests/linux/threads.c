/* CLONE_VM threads: shared memory, separate stack, TLS and futex join.
 *
 *   linuxrun /sfs/linux-threads
 *
 * Expected: "threads ok" and exit 0.
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

static long sys4(long n, long a, long b, long c, long d) {
    return sys6(n, a, b, c, d, 0, 0);
}

static long sys3(long n, long a, long b, long c) {
    return sys6(n, a, b, c, 0, 0, 0);
}

static long sys2(long n, long a, long b) { return sys6(n, a, b, 0, 0, 0, 0); }

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

static volatile unsigned long futex_word;
static volatile unsigned long thread_ran;
static volatile unsigned long tls_ok;
static unsigned long tls_block[8];
static volatile int parent_tid_word;
static volatile int child_tid_word;

static void thread_main(void) {
    unsigned long tls;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(tls));
    tls_ok = (tls == (unsigned long)tls_block);
    thread_ran = 1;
    futex_word = 1;
    sys4(98, (long)&futex_word, 1 /* FUTEX_WAKE */, 1, 0);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}

void _start(void) {
    const long SIZE = 64 * 1024;
    const unsigned long CLONE_VM = 0x00000100;
    const unsigned long CLONE_SIGHAND = 0x00000800;
    const unsigned long CLONE_THREAD = 0x00010000;
    const unsigned long CLONE_SETTLS = 0x00080000;
    const unsigned long CLONE_PARENT_SETTID = 0x00100000;
    const unsigned long CLONE_CHILD_SETTID = 0x01000000;
    const unsigned long CLONE_CHILD_CLEARTID = 0x00200000;

    unsigned char *stack = (unsigned char *)sys6(
        222, 0, SIZE, 3 /* READ|WRITE */, 0x22 /* PRIVATE|ANON */, -1, 0);
    if ((long)stack < 0) {
        fail("thread stack mmap failed\n", 25);
    }
    unsigned long stack_top = ((unsigned long)stack + SIZE) & ~15UL;
    tls_block[0] = 0x5A5A5A5AUL;

    long tid = sys6(220,
                    CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS |
                        CLONE_PARENT_SETTID | CLONE_CHILD_SETTID |
                        CLONE_CHILD_CLEARTID,
                    (long)stack_top, (long)&parent_tid_word,
                    (long)tls_block, (long)&child_tid_word, 0);
    if (tid < 0) {
        fail("clone thread failed\n", 19);
    }
    if (tid == 0) {
        thread_main();
    }

    /* Wait for the shared word and the worker's memory writes. */
    while (futex_word == 0) {
        sys4(98, (long)&futex_word, 0 /* FUTEX_WAIT */, 0, 0);
    }
    if (thread_ran != 1 || tls_ok != 1) {
        fail("thread handshake failed\n", 24);
    }
    if (parent_tid_word != (int)tid) {
        fail("parent tid wrong\n", 16);
    }

    /* Join: the kernel clears CLONE_CHILD_CLEARTID and wakes this futex. */
    while (child_tid_word != 0) {
        int expected = child_tid_word;
        sys4(98, (long)&child_tid_word, 0 /* FUTEX_WAIT */, expected, 0);
    }

    sys3(64, 1, (long)"threads ok\n", 11);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
