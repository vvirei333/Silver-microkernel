/* memfd fexecve probe: copy /sfs/linux-hello into an anonymous file, then
 * enter capability mode and execute that descriptor with AT_EMPTY_PATH. */
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
static long sys2(long n, long a, long b) { return sys5(n, a, b, 0, 0, 0); }
static long sys1(long n, long a) { return sys5(n, a, 0, 0, 0, 0); }

#define SYS_READ 63
#define SYS_WRITE 64
#define SYS_LSEEK 62
#define SYS_OPENAT 56
#define SYS_MEMFD_CREATE 279
#define SYS_EXECVEAT 281
#define SYS_EXIT 93
#define AT_FDCWD (-100L)
#define AT_EMPTY_PATH 0x1000L
#define SYS_SILVER_CAP 0x534c5601L

static int fail(const char *m, long n) {
    sys3(SYS_WRITE, 1, (long)m, n);
    sys1(SYS_EXIT, 1);
    for (;;) {}
}

void _start(void) {
    char path[] = "/sfs/linux-hello";
    long source = sys5(SYS_OPENAT, AT_FDCWD, (long)path, 0, 0, 0);
    if (source < 0) fail("memfd fexec: open failed\n", 25);
    long memfd = sys2(SYS_MEMFD_CREATE, (long)"elf", 0);
    if (memfd < 0) fail("memfd fexec: memfd_create failed\n", 33);

    char buffer[4096];
    for (;;) {
        long count = sys3(SYS_READ, source, (long)buffer, sizeof(buffer));
        if (count < 0) fail("memfd fexec: read failed\n", 25);
        if (count == 0) break;
        long written = 0;
        while (written < count) {
            long chunk = sys3(SYS_WRITE, memfd, (long)buffer + written, count - written);
            if (chunk <= 0) fail("memfd fexec: write failed\n", 26);
            written += chunk;
        }
    }
    if (sys3(SYS_LSEEK, memfd, 0, 0) != 0) fail("memfd fexec: seek failed\n", 25);

    long argv[] = { (long)"memfd", 0 };
    long envp[] = { 0 };
    if (sys3(SYS_SILVER_CAP, 0, 0, 0) != 0) fail("memfd fexec: cap_enter failed\n", 30);
    sys5(SYS_EXECVEAT, memfd, (long)"", (long)argv, (long)envp, AT_EMPTY_PATH);
    fail("memfd fexec: descriptor exec failed\n", 36);
}
