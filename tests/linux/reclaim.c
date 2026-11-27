/* Stale object-page reclaim after a forked child exits.
 *
 *   linuxrun /sfs/linux-reclaim
 *
 * Expected: "reclaim ok" and exit 0. A lazy mapping is populated, forked
 * once so the child holds a reference to the backing VmObject, and the child
 * exits. The parent then writes every page: each first write breaks one page
 * out of the object. Once the parent is the last user, the now-shadowed
 * object pages must be released instead of accumulating until munmap.
 * sysinfo(2) checks that the free-frame count does not fall by a whole
 * mapping; without pruning this fails with "stale pages retained".
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
static long sys1(long n, long a) { return sys6(n, a, 0, 0, 0, 0, 0); }
static long sys3(long n, long a, long b, long c) { return sys6(n, a, b, c, 0, 0, 0); }
static long sys5(long n, long a, long b, long c, long d, long e) {
    return sys6(n, a, b, c, d, e, 0);
}
static void fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys1(93, 1);
    for (;;) {
    }
}
void _start(void) {
    const long SIZE = 64 * 4096;
    unsigned char *map =
        (unsigned char *)sys6(222, 0, SIZE, 3, 0x22, -1, 0);
    if ((long)map < 0) {
        fail("mmap failed\n", 11);
    }
    for (long i = 0; i < SIZE; i += 4096) {
        map[i] = 0x5a;
    }
    for (long i = 0; i < SIZE; i += 4096) {
        if (map[i] != 0x5a) {
            fail("touch failed\n", 12);
        }
    }

    long pid = sys5(220, 17, 0, 0, 0, 0);
    if (pid < 0) {
        fail("fork failed\n", 11);
    }
    if (pid == 0) {
        sys1(93, 0);
        for (;;) {
        }
    }
    int status = 0;
    if (sys6(260, pid, (long)&status, 0, 0, 0, 0) != pid) {
        fail("wait failed\n", 11);
    }

    unsigned long before[14];
    unsigned long after[14];
    for (int i = 0; i < 14; i++) {
        before[i] = 0;
        after[i] = 0;
    }
    if (sys1(179, (long)before) != 0) {
        fail("sysinfo failed\n", 14);
    }
    for (long i = 0; i < SIZE; i += 4096) {
        map[i] = 0xa5;
    }
    map[0] = 0xa6;
    for (long i = 0; i < SIZE; i += 4096) {
        if (map[i] != (i == 0 ? 0xa6 : 0xa5)) {
            fail("data failed\n", 11);
        }
    }
    if (sys1(179, (long)after) != 0) {
        fail("sysinfo failed\n", 14);
    }
    /* A retained mapping would lose 64 frames. Allow 32 pages of unrelated
     * allocator noise, which still catches the regression. */
    if (after[5] + 32 * 4096 < before[5]) {
        fail("stale pages retained\n", 20);
    }
    if (sys6(215, (long)map, SIZE, 0, 0, 0, 0) != 0) {
        fail("munmap failed\n", 13);
    }
    sys3(64, 1, (long)"reclaim ok\n", 11);
    sys1(93, 0);
    for (;;) {
    }
}