/* fexecve probe: after cap_enter, path exec is gone but the open
 * descriptor can still be executed through execveat(AT_EMPTY_PATH).
 * On success this becomes /sfs/linux-hello, which exits 42. */
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
static long sys3(long n, long a, long b, long c) { return sys5(n, a, b, c, 0, 0); }
static long sys1(long n, long a) { return sys5(n, a, 0, 0, 0, 0); }

#define SYS_WRITE 64
#define SYS_OPENAT 56
#define SYS_EXECVEAT 281
#define SYS_EXIT 93
#define AT_FDCWD (-100L)
#define AT_EMPTY_PATH 0x1000L
#define SYS_SILVER_CAP 0x534c5601L
#define EPERM 1
#define ENOENT 2

static int fail(const char *m, long n) {
    sys3(SYS_WRITE, 1, (long)m, n);
    sys1(SYS_EXIT, 1);
    for (;;) {}
}

void _start(void) {
    char path[] = "/sfs/linux-hello";
    long fd = sys5(SYS_OPENAT, AT_FDCWD, (long)path, 0, 0, 0);
    if (fd < 0) fail("fexec: open failed\n", 19);

    long argv[] = { (long)"/sfs/linux-hello", 0 };
    long envp[] = { 0 };
    if (sys3(SYS_SILVER_CAP, 0, 0, 0) != 0) fail("fexec: cap_enter failed\n", 24);
    if (sys5(SYS_EXECVEAT, AT_FDCWD, (long)path, (long)argv, (long)envp, 0) != -EPERM)
        fail("fexec: cwd exec allowed\n", 24);
    if (sys5(SYS_EXECVEAT, fd, (long)"", (long)argv, (long)envp, 0) != -ENOENT)
        fail("fexec: empty path without flag\n", 31);
    sys5(SYS_EXECVEAT, fd, (long)"", (long)argv, (long)envp, AT_EMPTY_PATH);
    fail("fexec: descriptor exec failed\n", 30);
}
