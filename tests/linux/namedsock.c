/* Named AF_UNIX sockets: socket/bind/listen/accept/connect.
 *
 *   linuxrun /sfs/linux-namedsock
 *
 * Expected: "named socket ok" and exit 0.
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

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

struct sockaddr_un {
    unsigned short family;
    char path[108];
};

static const char NAME[] = "/sfs/wayland-test";
static const long NAME_LENGTH = sizeof(unsigned short) + sizeof(NAME);

static long make_address(struct sockaddr_un *address) {
    for (unsigned long index = 0; index < sizeof(address->path); index++) {
        address->path[index] = 0;
    }
    address->family = 1; /* AF_UNIX */
    for (unsigned long index = 0; index < sizeof(NAME); index++) {
        address->path[index] = NAME[index];
    }
    return NAME_LENGTH;
}

void _start(void) {
    long server = sys3(198 /* socket */, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0);
    if (server < 0) {
        fail("socket failed\n", 14);
    }
    struct sockaddr_un address;
    long length = make_address(&address);
    if (sys3(200 /* bind */, server, (long)&address, length) < 0) {
        fail("bind failed\n", 12);
    }

    /* The same path cannot be bound twice. */
    long duplicate = sys3(198, 1, 1, 0);
    if (duplicate < 0 || sys3(200, duplicate, (long)&address, length) != -98 /* EADDRINUSE */) {
        fail("duplicate bind\n", 15);
    }
    sys3(57 /* close */, duplicate, 0, 0);

    if (sys3(201 /* listen */, server, 8, 0) < 0) {
        fail("listen failed\n", 14);
    }

    long child = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (child < 0) {
        fail("fork failed\n", 12);
    }
    if (child == 0) {
        long client = sys3(198, 1, 1, 0);
        if (client < 0 || sys3(203 /* connect */, client, (long)&address, length) < 0) {
            fail("connect failed\n", 15);
        }
        if (sys3(64, client, (long)"hello", 5) != 5) {
            fail("send failed\n", 12);
        }
        sys3(93, 0, 0, 0);
        for (;;) {
        }
    }

    long connection = sys3(202 /* accept */, server, 0, 0);
    if (connection < 0) {
        fail("accept failed\n", 14);
    }
    char buffer[8];
    if (sys3(63, connection, (long)buffer, 8) != 5 || buffer[0] != 'h' || buffer[4] != 'o') {
        fail("receive failed\n", 15);
    }
    int status = 0;
    if (sys6(260 /* wait4 */, child, (long)&status, 0, 0, 0, 0) != child) {
        fail("wait failed\n", 12);
    }

    /* getsockname returns the bound path. */
    struct sockaddr_un bound;
    unsigned int bound_length = sizeof(bound);
    if (sys3(204 /* getsockname */, server, (long)&bound, (long)&bound_length) < 0 ||
        bound.family != 1 || bound_length != (unsigned int)NAME_LENGTH) {
        fail("getsockname failed\n", 19);
    }
    for (unsigned long index = 0; index < sizeof(NAME); index++) {
        if (bound.path[index] != NAME[index]) {
            fail("wrong name\n", 11);
        }
    }

    /* Connecting to a name nobody bound is refused. */
    long missing = sys3(198, 1, 1, 0);
    struct sockaddr_un absent;
    for (unsigned long index = 0; index < sizeof(absent.path); index++) {
        absent.path[index] = 0;
    }
    absent.family = 1;
    const char NONE[] = "/sfs/not-here";
    for (unsigned long index = 0; index < sizeof(NONE); index++) {
        absent.path[index] = NONE[index];
    }
    if (sys3(203, missing, (long)&absent, sizeof(short) + sizeof(NONE)) != -111 /* ECONNREFUSED */) {
        fail("missing name accepted\n", 22);
    }

    sys3(64, 1, (long)"named socket ok\n", 16);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
