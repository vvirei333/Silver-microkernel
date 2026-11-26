/* Leader exit must not destroy the address space of running workers.
 *
 *   linuxrun /sfs/linux-leader-exit
 *
 * Freestanding counterpart of the glibc pthread_lifetime probe: the leader
 * exits with SYS_EXIT (93) while a CLONE_VM worker is asleep. The worker then
 * touches its mapped page, prints and calls SYS_EXIT_GROUP (94). A kernel that
 * releases the leader's ttbr0 as soon as the leader dies faults here.
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

static volatile unsigned long ready;
static volatile unsigned long touched;
static char *page;

static void worker(void) {
    struct timespec {
        long seconds;
        long nanoseconds;
    } delay = {1, 0};
    print("worker running\n", 15);
    sys6(101, (long)&delay, 0, 0, 0, 0, 0);
    ready = 1;
    page[0] = 'o';
    page[1] = 'k';
    page[2] = '!';
    touched = 1;
    print("worker survived\n", 16);
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
    page = (char *)sys6(222, 0, 4096, 3, 0x22, -1, 0);
    if ((long)stack < 0 || (long)page < 0) {
        print("mmap failed\n", 11);
        sys3(93, 2, 0, 0);
    }
    unsigned long stack_top = ((unsigned long)stack + SIZE) & ~15UL;
    static unsigned long tls_block[8];
    static volatile int parent_tid_word;
    static volatile int child_tid_word;

    long tid = sys6(220,
                    CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS |
                        CLONE_PARENT_SETTID | CLONE_CHILD_SETTID |
                        CLONE_CHILD_CLEARTID,
                    (long)stack_top, (long)&parent_tid_word,
                    (long)tls_block, (long)&child_tid_word, 0);
    if (tid < 0) {
        print("clone failed\n", 13);
        sys3(93, 2, 0, 0);
    }
    if (tid == 0) {
        worker();
    }
    print("leader exiting\n", 15);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
