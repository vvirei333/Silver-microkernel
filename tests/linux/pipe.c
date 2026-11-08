/* pipe2 + fork + ppoll + read/write + wait4, freestanding.
 *
 *   linuxrun /sfs/linux-pipe
 *
 * The child writes "ping\n" into a pipe; the parent polls the read end, reads
 * it, echoes "pipe: ping" and waits. Expected output: "pipe: ping", parent
 * exits 0.
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

struct pollfd {
    int fd;
    short events;
    short revents;
};

struct timespec {
    long seconds;
    long nanoseconds;
};

void _start(void) {
    int fds[2];
    if (sys3(59, (long)fds, 0, 0) != 0) { /* pipe2 */
        sys3(64, 1, (long)"pipe failed\n", 12);
        sys3(93, 2, 0, 0);
        for (;;) {
        }
    }

    long pid = sys5(220, 17, 0, 0, 0, 0); /* clone(SIGCHLD) */
    if (pid == 0) {
        char message[] = "ping\n";
        sys3(57, fds[0], 0, 0);              /* close read end */
        sys3(64, fds[1], (long)message, 5);  /* write */
        sys3(57, fds[1], 0, 0);
        sys3(93, 0, 0, 0);
        for (;;) {
        }
    }
    if (pid < 0) {
        sys3(64, 1, (long)"fork failed\n", 12);
        sys3(93, 3, 0, 0);
        for (;;) {
        }
    }

    sys3(57, fds[1], 0, 0); /* close write end in the parent */

    struct pollfd poll = {fds[0], 1 /* POLLIN */, 0};
    struct timespec timeout = {2, 0};
    long ready = sys4(73, (long)&poll, 1, (long)&timeout, 0); /* ppoll */
    if (ready != 1 || (poll.revents & 1) == 0) {
        sys3(64, 1, (long)"poll failed\n", 12);
        sys3(93, 4, 0, 0);
        for (;;) {
        }
    }

    char buffer[16];
    long count = sys3(63, fds[0], (long)buffer, sizeof(buffer)); /* read */
    if (count != 5) {
        sys3(64, 1, (long)"read failed\n", 12);
        sys3(93, 5, 0, 0);
        for (;;) {
        }
    }
    sys3(57, fds[0], 0, 0);

    sys3(64, 1, (long)"pipe: ", 6);
    sys3(64, 1, (long)buffer, count);

    int status = 0;
    long waited = sys4(260, pid, (long)&status, 0, 0); /* wait4 */
    if (waited != pid || ((status >> 8) & 0xff) != 0) {
        sys3(64, 1, (long)"child failed\n", 13);
        sys3(93, 6, 0, 0);
        for (;;) {
        }
    }
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
