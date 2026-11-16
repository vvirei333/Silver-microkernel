/* Per-task Linux read scope under the Alpine root mapping.
 *
 *   linuxrun /sfs/linux-scope
 *
 * The Linux root is the SFS mount, so `/readme.txt` and `/sfs/readme.txt` are
 * the same file and `/bin` is a real directory of the rootfs. The scope guard
 * must still refuse anything that resolves *outside* the mount, which is what
 * the `/sfs/..` alias does in the kernel tree.
 *
 * Expected: "scope ok" and exit 0.
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

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

void _start(void) {
    const long AT_FDCWD = -100;
    const long EACCES = 13;

    /* The root mapping: a rootfs path and its legacy alias name one file. */
    long mapped = sys4(56, AT_FDCWD, (long)"/readme.txt", 0, 0);
    if (mapped < 0) {
        fail("root mapping failed\n", 19);
    }
    sys1(57, mapped);

    long allowed = sys4(56, AT_FDCWD, (long)"/sfs/readme.txt", 0, 0);
    if (allowed < 0) {
        fail("allowed read failed\n", 20);
    }
    sys1(57, allowed);

    /* `/sfs/..` climbs out of the mount in the kernel tree; the Linux task
     * must not see what is up there. */
    long escaped = sys4(56, AT_FDCWD, (long)"/sfs/../readme.txt", 0, 0);
    if (escaped != -EACCES) {
        fail("read scope leaked\n", 18);
    }

    if (sys2(49, (long)"/sfs/..", 0) != -EACCES) {
        fail("chdir escaped scope\n", 21);
    }
    if (sys2(49, (long)"/sfs", 0) != 0) {
        fail("chdir /sfs failed\n", 17);
    }

    sys3(64, 1, (long)"scope ok\n", 9);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
