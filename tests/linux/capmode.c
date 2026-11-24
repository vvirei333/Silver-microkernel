/* Capsicum-lite Linux ABI probe.
 *
 * Pre-cap: descriptor rights are readable and only narrow.
 * After cap_enter: path syscalls fail; fd-relative open still works while
 * the directory retains FD_LOOKUP; already-open descriptors keep working.
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
static long sys4(long n, long a, long b, long c, long d) { return sys5(n,a,b,c,d,0); }
static long sys3(long n, long a, long b, long c) { return sys5(n,a,b,c,0,0); }
static long sys2(long n, long a, long b) { return sys5(n,a,b,0,0,0); }
static long sys1(long n, long a) { return sys5(n,a,0,0,0,0); }

#define SYS_READ 63
#define SYS_WRITE 64
#define SYS_CLOSE 57
#define SYS_DUP 23
#define SYS_LSEEK 62
#define SYS_FSTAT 80
#define SYS_OPENAT 56
#define SYS_MKDIRAT 34
#define SYS_CHDIR 49
#define SYS_EXECVE 221
#define SYS_EXIT 93
#define AT_FDCWD (-100L)
#define O_DIRECTORY 0x10000L
#define FD_READ 1
#define FD_WRITE 2
#define FD_SEEK 4
#define FD_FSTAT 8
#define FD_TRUNCATE 64
#define SYS_SILVER_CAP 0x534c5601L
#define EPERM 1
#define EINVAL 22

static int fail(const char *m, long n) {
    sys3(SYS_WRITE, 1, (long)m, n);
    sys1(SYS_EXIT, 1);
    for (;;) {}
}

void _start(void) {
    const char *path = "/sfs/linux-hello";
    long fd = sys4(SYS_OPENAT, AT_FDCWD, (long)path, 0, 0);
    if (fd < 0) fail("capmode: open before enter failed\n", 33);

    long r = sys3(SYS_SILVER_CAP, 2, fd, 0);
    if (r < 0 || (r & (FD_READ|FD_SEEK|FD_FSTAT)) != (FD_READ|FD_SEEK|FD_FSTAT))
        fail("capmode: initial rights wrong\n", 32);
    if (r & (FD_WRITE|FD_TRUNCATE)) fail("capmode: writable read-only fd\n", 33);

    long kept = FD_READ | FD_FSTAT;
    if (sys3(SYS_SILVER_CAP, 1, fd, kept) != 0) fail("capmode: narrow failed\n", 27);
    char buf[4];
    char statbuf[256];
    if (sys3(SYS_READ, fd, (long)buf, 4) != 4) fail("capmode: read broke\n", 22);
    if (sys3(SYS_LSEEK, fd, 0, 0) != -EPERM) fail("capmode: seek not denied\n", 27);
    if (sys2(SYS_FSTAT, fd, (long)statbuf) != 0) fail("capmode: fstat broke\n", 22);
    if (sys3(SYS_WRITE, fd, (long)"x", 1) != -EPERM) fail("capmode: write not denied\n", 27);
    if (sys3(SYS_SILVER_CAP, 1, fd, FD_READ|FD_FSTAT|FD_WRITE) != 0)
        fail("capmode: widen call failed\n", 30);
    if (sys3(SYS_SILVER_CAP, 2, fd, 0) != kept) fail("capmode: rights widened\n", 26);
    long dupfd = sys1(SYS_DUP, fd);
    if (dupfd < 0) fail("capmode: dup failed\n", 23);
    if (sys3(SYS_SILVER_CAP, 2, dupfd, 0) != kept) fail("capmode: dup lost rights\n", 27);
    sys1(SYS_CLOSE, dupfd);
    if (sys3(SYS_SILVER_CAP, 1, fd, 0x80000000L) != -EINVAL)
        fail("capmode: unknown bits accepted\n", 33);

    long dir = sys4(SYS_OPENAT, AT_FDCWD, (long)"/sfs", O_DIRECTORY, 0);
    if (dir < 0) fail("capmode: dir open failed\n", 29);

    if (sys3(SYS_SILVER_CAP, 0, 0, 0) != 0) fail("capmode: enter failed\n", 27);
    if (sys4(SYS_OPENAT, AT_FDCWD, (long)path, 0, 0) != -EPERM)
        fail("capmode: cwd open allowed\n", 29);
    if (sys2(SYS_CHDIR, (long)"/", 0) != -EPERM)
        fail("capmode: chdir allowed\n", 25);
    long m = sys4(SYS_MKDIRAT, AT_FDCWD, (long)"/tmp/capdir", 0755, 0);
    if (m != -EPERM) fail("capmode: mkdir allowed\n", 25);
    long argv[] = { (long)"/sfs/linux-hello", 0 };
    long envp[] = { 0 };
    if (sys3(SYS_EXECVE, (long)path, (long)argv, (long)envp) != -EPERM)
        fail("capmode: execve allowed\n", 26);

    long via_dir = sys4(SYS_OPENAT, dir, (long)"linux-hello", 0, 0);
    if (via_dir < 0) fail("capmode: dirfd open denied\n", 31);
    if (sys3(SYS_READ, via_dir, (long)buf, 4) != 4) fail("capmode: dirfd read broke\n", 30);
    sys1(SYS_CLOSE, via_dir);

    if (sys2(SYS_FSTAT, fd, (long)statbuf) != 0) fail("capmode: fstat after enter\n", 31);
    sys3(SYS_WRITE, 1, (long)"capmode ok\n", 11);
    sys1(SYS_EXIT, 0);
    for (;;) {}
}
