/* AF_UNIX socketpair, SCM_RIGHTS and EOF.
 *
 *   linuxrun /sfs/linux-unixsock
 *
 * Expected: "unix socket ok" and exit 0.
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

static long sys2(long n, long a, long b) { return sys6(n, a, b, 0, 0, 0, 0); }

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

struct iovec {
    void *base;
    unsigned long length;
};

struct msghdr {
    void *name;
    unsigned int name_length;
    unsigned int pad;
    struct iovec *iov;
    unsigned long iov_length;
    void *control;
    unsigned long control_length;
    int flags;
    int pad2;
};

struct cmsghdr {
    unsigned long length;
    int level;
    int kind;
};

union control_buffer {
    struct cmsghdr header;
    unsigned char raw[64];
};

static const int SOL_SOCKET = 1;
static const int SCM_RIGHTS = 1;

void _start(void) {
    int pair[2];
    if (sys6(199, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, (long)pair, 0, 0) < 0) {
        fail("socketpair failed\n", 18);
    }

    /* Byte stream through the pair. */
    if (sys3(64, pair[0], (long)"ping", 4) != 4) {
        fail("send failed\n", 12);
    }
    char buffer[8];
    if (sys3(63, pair[1], (long)buffer, 8) != 4 || buffer[0] != 'p' || buffer[3] != 'g') {
        fail("recv failed\n", 12);
    }

    /* Pass an eventfd with SCM_RIGHTS. */
    long event = sys2(19 /* eventfd2 */, 0, 0);
    if (event < 0) {
        fail("eventfd failed\n", 15);
    }
    struct iovec vector = { "x", 1 };
    union control_buffer control;
    struct cmsghdr *header = &control.header;
    header->length = sizeof(struct cmsghdr) + sizeof(int);
    header->level = SOL_SOCKET;
    header->kind = SCM_RIGHTS;
    *(int *)((unsigned char *)header + sizeof(struct cmsghdr)) = (int)event;

    struct msghdr outgoing;
    outgoing.name = 0;
    outgoing.name_length = 0;
    outgoing.pad = 0;
    outgoing.iov = &vector;
    outgoing.iov_length = 1;
    outgoing.control = control.raw;
    outgoing.control_length = sizeof(struct cmsghdr) + sizeof(int);
    outgoing.flags = 0;
    outgoing.pad2 = 0;
    if (sys3(211 /* sendmsg */, pair[0], (long)&outgoing, 0) != 1) {
        fail("sendmsg failed\n", 15);
    }

    union control_buffer received;
    char payload[8];
    struct iovec incoming_vector = { payload, sizeof(payload) };
    struct msghdr incoming;
    incoming.name = 0;
    incoming.name_length = 0;
    incoming.pad = 0;
    incoming.iov = &incoming_vector;
    incoming.iov_length = 1;
    incoming.control = received.raw;
    incoming.control_length = sizeof(received.raw);
    incoming.flags = 0;
    incoming.pad2 = 0;
    long count = sys3(212 /* recvmsg */, pair[1], (long)&incoming, 0);
    if (count != 1 || payload[0] != 'x') {
        fail("recvmsg failed\n", 15);
    }
    struct cmsghdr *got = &received.header;
    if (got->length != sizeof(struct cmsghdr) + sizeof(int) ||
        got->level != SOL_SOCKET || got->kind != SCM_RIGHTS) {
        fail("control wrong\n", 14);
    }
    int passed = *(int *)((unsigned char *)got + sizeof(struct cmsghdr));
    if (passed < 3) {
        fail("no descriptor\n", 14);
    }
    unsigned long one = 1;
    if (sys3(64, passed, (long)&one, 8) != 8) {
        fail("passed fd write\n", 16);
    }
    unsigned long counter = 0;
    if (sys3(63, event, (long)&counter, 8) != 8 || counter != 1) {
        fail("fd not shared\n", 14);
    }

    /* O_NONBLOCK through fcntl and a closed writer produce EAGAIN then EOF. */
    if (sys3(25 /* fcntl */, pair[1], 4 /* F_SETFL */, 0x800 /* O_NONBLOCK */) < 0) {
        fail("fcntl failed\n", 13);
    }
    if (sys3(63, pair[1], (long)buffer, 8) != -11 /* EAGAIN */) {
        fail("nonblock read\n", 15);
    }
    if (sys3(210 /* shutdown */, pair[0], 1 /* SHUT_WR */, 0) < 0) {
        fail("shutdown failed\n", 16);
    }
    if (sys3(63, pair[1], (long)buffer, 8) != 0) {
        fail("missing EOF\n", 12);
    }

    sys3(64, 1, (long)"unix socket ok\n", 15);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
