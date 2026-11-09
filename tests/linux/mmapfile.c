/* File-backed mmap + mprotect + MAP_FIXED.
 *
 *   linuxrun /sfs/linux-mmapfile
 *
 * Expected: "mmap ok" and exit 0.
 *
 * big.bin is 3 MiB; byte i is `i & 0xff`. The test maps it MAP_PRIVATE,
 * checks bytes far apart, mprotects one page writable, maps a second region
 * with MAP_FIXED over a PROT_NONE reservation, and unmaps both.
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

static long sys1(long n, long a) { return sys6(n, a, 0, 0, 0, 0, 0); }

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

void _start(void) {
    const long AT_FDCWD = -100;
    const long PROT_READ = 1;
    const long PROT_WRITE = 2;
    const long PROT_NONE = 0;
    const long MAP_SHARED = 0x01;
    const long MAP_PRIVATE = 0x02;
    const long MAP_FIXED = 0x10;
    const long MAP_ANONYMOUS = 0x20;
    const long SIZE = 3 * 1024 * 1024;

    long fd = sys4(56, AT_FDCWD, (long)"/sfs/big.bin", 0, 0);
    if (fd < 0) {
        fail("open big.bin failed\n", 20);
    }

    unsigned char *mapped =
        (unsigned char *)sys6(222, 0, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    if ((long)mapped < 0) {
        fail("file mmap failed\n", 17);
    }
    if (mapped[0] != 0 || mapped[1] != 1 ||
        mapped[1024 * 1024] != 0 || mapped[1024 * 1024 + 7] != 7 ||
        mapped[SIZE - 1] != (unsigned char)(SIZE - 1)) {
        fail("file mmap content wrong\n", 24);
    }

    /* One page becomes writable, then read-only again. */
    if (sys3(226, (long)mapped + 4096, 4096, PROT_READ | PROT_WRITE) != 0) {
        fail("mprotect rw failed\n", 19);
    }
    mapped[4096] = 0x5a;
    if (mapped[4096] != 0x5a) {
        fail("mprotect write failed\n", 22);
    }
    if (sys3(226, (long)mapped + 4096, 4096, PROT_READ) != 0) {
        fail("mprotect ro failed\n", 19);
    }

    /* Reserve a range, then replace it with MAP_FIXED. glibc's ld.so does
     * exactly this: PROT_NONE over the whole library span, then one fixed
     * mapping per PT_LOAD segment. */
    unsigned char *reserved =
        (unsigned char *)sys6(222, 0, 64 * 1024, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if ((long)reserved < 0) {
        fail("reserve mmap failed\n", 20);
    }
    unsigned char *fixed =
        (unsigned char *)sys6(222, (long)reserved, 64 * 1024, PROT_READ,
                              MAP_PRIVATE | MAP_FIXED, fd, 0);
    if (fixed != reserved) {
        fail("MAP_FIXED moved\n", 16);
    }
    if (fixed[0] != 0 || fixed[1] != 1 || fixed[4096] != 0) {
        fail("MAP_FIXED content wrong\n", 24);
    }

    if (sys2(215, (long)mapped, SIZE) != 0) {
        fail("munmap file failed\n", 19);
    }
    if (sys2(215, (long)reserved, 64 * 1024) != 0) {
        fail("munmap fixed failed\n", 20);
    }
    sys1(57, fd);

    sys3(64, 1, (long)"mmap ok\n", 8);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
