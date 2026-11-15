/* Security regression: Linux ABI fd lifetime across SCM_RIGHTS, 1000 rounds.
 *
 *   linuxrun /sfs/sec-linux-fd-lifetime
 *
 * The M0.2 class is a descriptor remembered by number instead of owned:
 * close the sender's copy, recycle that number with an unrelated file, and
 * the receiver reads the wrong bytes. Each round creates a memfd with a
 * round-specific pattern, passes it over a socketpair, closes the sender's
 * descriptor, churns new memfds through the freed number, and requires the
 * received descriptor to still read the original pattern. A number-based
 * table would read 0xEE from a churn file.
 *
 * Expected: "linux fd lifetime ok" and exit 0.
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

static __attribute__((noreturn)) void fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    for (;;) {
    }
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

static const long SYS_CLOSE = 57;
static const long SYS_WRITE = 64;
static const long SYS_PREAD = 67;
static const long SYS_EXIT = 93;
static const long SYS_FTRUNCATE = 46;
static const long SYS_SOCKETPAIR = 199;
static const long SYS_SENDMSG = 211;
static const long SYS_RECVMSG = 212;
static const long SYS_MEMFD_CREATE = 279;
static const int SOL_SOCKET = 1;
static const int SCM_RIGHTS = 1;

#define PAYLOAD 64
#define CHURN 8
#define ROUNDS 1000

static unsigned char original[PAYLOAD];
static unsigned char churn_data[PAYLOAD];

static int pass_descriptor(int source, int *pair) {
    if (sys6(SYS_SOCKETPAIR, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0,
             (long)pair, 0, 0) < 0) {
        return 0;
    }

    char payload = 'F';
    struct iovec outgoing_iov = { &payload, 1 };
    union control_buffer outgoing_control;
    struct cmsghdr *out_header = &outgoing_control.header;
    out_header->length = sizeof(struct cmsghdr) + sizeof(int);
    out_header->level = SOL_SOCKET;
    out_header->kind = SCM_RIGHTS;
    *(int *)(outgoing_control.raw + sizeof(struct cmsghdr)) = source;
    struct msghdr outgoing = { 0, 0, 0, &outgoing_iov, 1,
                               &outgoing_control, out_header->length, 0, 0 };
    if (sys3(SYS_SENDMSG, pair[0], (long)&outgoing, 0) != 1) {
        return 0;
    }

    char received;
    struct iovec incoming_iov = { &received, 1 };
    union control_buffer incoming_control;
    struct msghdr incoming = { 0, 0, 0, &incoming_iov, 1,
                               &incoming_control, sizeof(incoming_control), 0, 0 };
    if (sys3(SYS_RECVMSG, pair[1], (long)&incoming, 0) < 0) {
        return 0;
    }
    struct cmsghdr *in_header = &incoming_control.header;
    if (in_header->level != SOL_SOCKET || in_header->kind != SCM_RIGHTS ||
        in_header->length < sizeof(struct cmsghdr) + sizeof(int)) {
        return 0;
    }
    return *(int *)(incoming_control.raw + sizeof(struct cmsghdr));
}

void _start(void) {
    for (int index = 0; index < PAYLOAD; index++) {
        churn_data[index] = 0xEE;
    }

    for (int round = 0; round < ROUNDS; round++) {
        for (int index = 0; index < PAYLOAD; index++) {
            original[index] = (unsigned char)(round * 13 + index);
        }

        int source = (int)sys3(SYS_MEMFD_CREATE, (long)"linux-fd-lifetime", 0, 0);
        if (source < 0) {
            fail("linux fd lifetime: memfd failed\n", 33);
        }
        if (sys3(SYS_FTRUNCATE, source, PAYLOAD, 0) < 0 ||
            sys3(SYS_WRITE, source, (long)original, PAYLOAD) != PAYLOAD) {
            fail("linux fd lifetime: write failed\n", 32);
        }

        int pair[2] = { -1, -1 };
        int passed = pass_descriptor(source, pair);
        if (passed < 0) {
            fail("linux fd lifetime: SCM_RIGHTS failed\n", 36);
        }

        /* The sender's reference is gone; only the kernel's duplicate for the
           receiver remains. */
        sys3(SYS_CLOSE, source, 0, 0);

        /* Recycle the freed number with unrelated files of different bytes. */
        int churn[CHURN];
        for (int index = 0; index < CHURN; index++) {
            churn[index] = (int)sys3(SYS_MEMFD_CREATE, (long)"linux-fd-churn", 0, 0);
            if (churn[index] >= 0) {
                if (sys3(SYS_FTRUNCATE, churn[index], PAYLOAD, 0) >= 0) {
                    sys3(SYS_WRITE, churn[index], (long)churn_data, PAYLOAD);
                }
            }
        }

        unsigned char check[PAYLOAD];
        if (sys6(SYS_PREAD, passed, (long)check, PAYLOAD, 0, 0, 0) != PAYLOAD) {
            fail("linux fd lifetime: pread failed\n", 32);
        }
        for (int index = 0; index < PAYLOAD; index++) {
            if (check[index] != original[index]) {
                fail("linux fd lifetime: descriptor recycled\n", 39);
            }
        }

        for (int index = 0; index < CHURN; index++) {
            if (churn[index] >= 0) {
                sys3(SYS_CLOSE, churn[index], 0, 0);
            }
        }
        sys3(SYS_CLOSE, passed, 0, 0);
        sys3(SYS_CLOSE, pair[0], 0, 0);
        sys3(SYS_CLOSE, pair[1], 0, 0);
    }

    sys3(64, 1, (long)"linux fd lifetime ok\n", 21);
    sys3(SYS_EXIT, 0, 0, 0);
    for (;;) {
    }
}
