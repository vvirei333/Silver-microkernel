/* eventfd2 + memfd_create + futex + ppoll, freestanding.
 *
 *   linuxrun /sfs/linux-ipc
 *
 * Expected: "ipc ok" and exit 0.
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

struct pollfd {
    int fd;
    short events;
    short revents;
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
    /* eventfd: write a counter, poll it, read it back. */
    long event = sys2(19, 0, 0); /* eventfd2(0, 0) */
    if (event < 0) {
        fail("eventfd failed\n", 15);
    }
    unsigned long one = 1;
    sys3(64, event, (long)&one, 8);
    struct pollfd poll = {(int)event, 1 /* POLLIN */, 0};
    struct timespec timeout = {1, 0};
    if (sys4(73, (long)&poll, 1, (long)&timeout, 0) != 1 ||
        (poll.revents & 1) == 0) {
        fail("eventfd poll failed\n", 20);
    }
    unsigned long value = 0;
    if (sys3(63, event, (long)&value, 8) != 8 || value != 1) {
        fail("eventfd read failed\n", 20);
    }
    sys1(57, event);

    /* memfd: write, seek, read, fstat. */
    long mem = sys2(279, (long)"scratch", 0); /* memfd_create */
    if (mem < 0) {
        fail("memfd failed\n", 13);
    }
    sys3(64, mem, (long)"abc", 3);
    if (sys3(62, mem, 0, 0) != 0) { /* lseek(fd, 0, SEEK_SET) */
        fail("memfd seek failed\n", 18);
    }
    char buffer[8];
    if (sys3(63, mem, (long)buffer, sizeof(buffer)) != 3 ||
        buffer[0] != 'a' || buffer[1] != 'b' || buffer[2] != 'c') {
        fail("memfd read failed\n", 18);
    }
    unsigned char stat[128];
    if (sys2(80, mem, (long)stat) != 0) { /* fstat */
        fail("memfd fstat failed\n", 19);
    }
    long size = 0;
    for (int index = 0; index < 8; index++) {
        size |= (long)stat[48 + index] << (index * 8);
    }
    if (size != 3) {
        fail("memfd size wrong\n", 17);
    }
    sys1(57, mem);

    /* futex: an uncontended WAIT with a 10 ms timeout reports ETIMEDOUT. */
    int lock = 0;
    struct timespec wait = {0, 10 * 1000 * 1000};
    long result = sys4(98, (long)&lock, 0 /* FUTEX_WAIT */, 0, (long)&wait);
    if (result != -110) {
        fail("futex result wrong\n", 18);
    }

    sys3(64, 1, (long)"ipc ok\n", 7);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
