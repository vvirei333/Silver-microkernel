/* Security regression: descriptor lifetime across SCM_RIGHTS.
 *
 *   linuxrun /sfs/sec-fd-lifetime
 *
 * The M0.2 post-mortem in this repository was a user-space fd-lifetime bug: a
 * Wayland proxy stored a wl_shm pool descriptor without owning it, the pool
 * was destroyed, the number was recycled by an unrelated file, and the proxy
 * mapped zeros. The same class exists in the kernel's unix-socket layer: a
 * descriptor sent with SCM_RIGHTS must be duplicated (refcounted), not moved
 * or remembered by number.
 *
 * This test:
 *   1. creates a memfd and fills it with a known pattern;
 *   2. passes it over a socketpair with SCM_RIGHTS;
 *   3. closes the sender's descriptor;
 *   4. churns enough new descriptors to recycle the freed number;
 *   5. reads the received descriptor and requires the original pattern.
 *
 * A kernel that stores only the number, or that closes the original when the
 * sender does, will read the churn file's bytes (0xEE) and fail.
 *
 * Expected: "fd lifetime ok" and exit 0.
 */
#define PAGE 4096

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

static unsigned char original[PAGE];
static unsigned char churn_data[PAGE];

void _start(void) {
    int index;
    for (index = 0; index < PAGE; index++) {
        original[index] = (unsigned char)(index * 7 + 13);
        churn_data[index] = 0xEE;
    }

    long source = sys3(SYS_MEMFD_CREATE, (long)"fd-lifetime-orig", 0, 0);
    if (source < 0) {
        fail("fd-lifetime: memfd failed\n", 26);
    }
    if (sys3(SYS_FTRUNCATE, source, PAGE, 0) < 0) {
        fail("fd-lifetime: ftruncate failed\n", 30);
    }
    if (sys3(SYS_WRITE, source, (long)original, PAGE) != PAGE) {
        fail("fd-lifetime: write failed\n", 26);
    }

    int pair[2];
    if (sys6(SYS_SOCKETPAIR, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0,
             (long)pair, 0, 0) < 0) {
        fail("fd-lifetime: socketpair failed\n", 31);
    }

    char payload = 'F';
    struct iovec outgoing_iov = { &payload, 1 };
    union control_buffer outgoing_control;
    struct cmsghdr *out_header = &outgoing_control.header;
    out_header->length = sizeof(struct cmsghdr) + sizeof(int);
    out_header->level = SOL_SOCKET;
    out_header->kind = SCM_RIGHTS;
    int *out_fd = (int *)(outgoing_control.raw + sizeof(struct cmsghdr));
    *out_fd = (int)source;
    struct msghdr outgoing = { 0, 0, 0, &outgoing_iov, 1,
                               &outgoing_control, out_header->length, 0, 0 };
    if (sys3(SYS_SENDMSG, pair[0], (long)&outgoing, 0) != 1) {
        fail("fd-lifetime: sendmsg failed\n", 28);
    }

    char received;
    struct iovec incoming_iov = { &received, 1 };
    union control_buffer incoming_control;
    struct msghdr incoming = { 0, 0, 0, &incoming_iov, 1,
                               &incoming_control, sizeof(incoming_control), 0, 0 };
    if (sys3(SYS_RECVMSG, pair[1], (long)&incoming, 0) < 0) {
        fail("fd-lifetime: recvmsg failed\n", 28);
    }
    struct cmsghdr *in_header = &incoming_control.header;
    if (in_header->level != SOL_SOCKET || in_header->kind != SCM_RIGHTS ||
        in_header->length < sizeof(struct cmsghdr) + sizeof(int)) {
        fail("fd-lifetime: no descriptor\n", 27);
    }
    int passed = *(int *)(incoming_control.raw + sizeof(struct cmsghdr));
    if (passed < 0) {
        fail("fd-lifetime: bad descriptor\n", 29);
    }

    /* Drop the sender's copy. From here the only reference is the one the
       kernel created for the receiver. */
    sys3(SYS_CLOSE, source, 0, 0);

    /* Recycle the freed number with unrelated files of different content. */
    int churn[48];
    for (index = 0; index < 48; index++) {
        churn[index] = (int)sys3(SYS_MEMFD_CREATE, (long)"fd-lifetime-churn", 0, 0);
        if (churn[index] < 0) {
            continue;
        }
        if (sys3(SYS_FTRUNCATE, churn[index], PAGE, 0) >= 0) {
            sys3(SYS_WRITE, churn[index], (long)churn_data, PAGE);
        }
    }

    unsigned char check[64];
    long got = sys6(SYS_PREAD, passed, (long)check, sizeof(check), 0, 0, 0);
    if (got != (long)sizeof(check)) {
        fail("fd-lifetime: read back failed\n", 30);
    }
    for (index = 0; index < (int)sizeof(check); index++) {
        if (check[index] != original[index]) {
            fail("fd-lifetime: descriptor recycled\n", 33);
        }
    }

    /* The received descriptor must still be independently closeable. */
    if (sys3(SYS_CLOSE, passed, 0, 0) < 0) {
        fail("fd-lifetime: close failed\n", 26);
    }

    sys3(64, 1, (long)"fd lifetime ok\n", 16);
    sys3(SYS_EXIT, 0, 0, 0);
    for (;;) {
    }
}
