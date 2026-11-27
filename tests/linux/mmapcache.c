/* File-backed page cache sharing and COW isolation.
 *
 *   linuxrun /sfs/linux-mmapcache
 *
 * Expected: "mmap cache ok" and exit 0.
 *
 * Two private mappings of the same 1 MiB slice of big.bin are faulted in.
 * The second mapping must reuse the cached file pages instead of allocating
 * another 256 frames. Writing every page of the second mapping must create
 * private COW pages without changing the first mapping or the cached bytes.
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
static void snapshot(unsigned long *info) {
    for (int i = 0; i < 14; i++) {
        info[i] = 0;
    }
    if (sys1(179, (long)info) != 0) {
        fail("sysinfo failed\n", 14);
    }
}
void _start(void) {
    const long AT_FDCWD = -100;
    const long PROT_READ = 1;
    const long PROT_WRITE = 2;
    const long MAP_PRIVATE = 0x02;
    const long SIZE = 1024 * 1024;
    const long PAGE = 4096;
    unsigned char *a;
    unsigned char *b;
    unsigned long before[14];
    unsigned long mid[14];
    unsigned long after_b[14];
    unsigned long after_c[14];
    unsigned long after_write[14];
    unsigned long end[14];

    long fd = sys4(56, AT_FDCWD, (long)"/sfs/big.bin", 0, 0);
    if (fd < 0) {
        fail("open big.bin failed\n", 20);
    }
    snapshot(before);

    a = (unsigned char *)sys6(222, 0, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    if ((long)a < 0) {
        fail("first mmap failed\n", 19);
    }
    for (long i = 0; i < SIZE; i += PAGE) {
        if (a[i] != (unsigned char)(i & 0xff)) {
            fail("first content wrong\n", 21);
        }
    }
    snapshot(mid);

    b = (unsigned char *)sys6(222, 0, SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if ((long)b < 0) {
        fail("second mmap failed\n", 20);
    }
    for (long i = 0; i < SIZE; i += PAGE) {
        if (b[i] != (unsigned char)(i & 0xff)) {
            fail("second content wrong\n", 22);
        }
    }
    snapshot(after_b);
    /* Without a shared object the second mapping allocates another 256
      frames. Allow 32 pages of page-table/allocator noise. */
    if (after_b[5] + 32 * PAGE < mid[5]) {
        fail("file cache miss\n", 17);
    }

    /* A mapping at a nonzero file offset must address the same absolute
      file pages, not a separate cache keyed by region offset. */
    const long OFFSET = 4096;
    const long C_SIZE = 64 * 1024;
    unsigned char *c = (unsigned char *)sys6(
        222, 0, C_SIZE, PROT_READ, MAP_PRIVATE, fd, OFFSET);
    if ((long)c < 0) {
        fail("offset mmap failed\n", 19);
    }
    if (c[0] != 0 || c[4095] != 0xff) {
        fail("offset content wrong\n", 21);
    }
    snapshot(after_c);
    if (after_c[5] + 16 * PAGE < after_b[5]) {
        fail("offset cache miss\n", 18);
    }
    if (sys2(215, (long)c, C_SIZE) != 0) {
        fail("munmap offset failed\n", 21);
    }

    /* A writable mapping whose first access is a store: the cached page
      exists but this address space has no PTE for it yet. */
    unsigned char *d = (unsigned char *)sys6(
        222, 0, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 8192);
    if ((long)d < 0) {
        fail("write-first mmap failed\n", 23);
    }
    d[0] = 0x5a;
    if (d[0] != 0x5a || a[8192] != (unsigned char)(8192 & 0xff)) {
        fail("write-first cached page\n", 24);
    }
    if (sys2(215, (long)d, PAGE) != 0) {
        fail("munmap write-first failed\n", 25);
    }

    for (long i = 0; i < SIZE; i += PAGE) {
        b[i] = (unsigned char)(0xc0 | ((i / PAGE) & 0x3f));
    }
    for (long i = 0; i < SIZE; i += PAGE) {
        if (b[i] != (unsigned char)(0xc0 | ((i / PAGE) & 0x3f))) {
            fail("cow write failed\n", 17);
        }
        if (a[i] != (unsigned char)(i & 0xff)) {
            fail("cache write leaked\n", 20);
        }
    }
    snapshot(after_write);

    if (sys2(215, (long)b, SIZE) != 0) {
        fail("munmap second failed\n", 22);
    }
    if (sys2(215, (long)a, SIZE) != 0) {
        fail("munmap first failed\n", 21);
    }
    snapshot(end);
    /* The weak cache entry must not pin the object once both mappings are
      gone. Allow page tables and unrelated allocator noise. */
    if (end[5] + 64 * PAGE < before[5]) {
        fail("file cache leaked\n", 19);
    }
    sys1(57, fd);

    /* A writable VFS file must invalidate its cache entry. Map the old
      contents, overwrite the file, then map again and expect the new byte. */
    const long O_RDWR = 2;
    const long O_CREAT = 0x40;
    const long O_TRUNC = 0x200;
    long tfd = sys4(56, AT_FDCWD, (long)"/tmp/mmapcache.dat",
                    O_RDWR | O_CREAT | O_TRUNC, 0x180);
    if (tfd < 0) {
        fail("tmp open failed\n", 16);
    }
    if (sys3(64, tfd, (long)"AAAA", 4) != 4) {
        fail("tmp write failed\n", 17);
    }
    unsigned char *old = (unsigned char *)sys6(
        222, 0, PAGE, PROT_READ, MAP_PRIVATE, tfd, 0);
    if ((long)old < 0 || old[0] != 'A') {
        fail("tmp mmap failed\n", 17);
    }
    if (sys4(68, tfd, (long)"BBBB", 4, 0) != 4) {
        fail("tmp pwrite failed\n", 18);
    }
    unsigned char *fresh = (unsigned char *)sys6(
        222, 0, PAGE, PROT_READ, MAP_PRIVATE, tfd, 0);
    if ((long)fresh < 0 || fresh[0] != 'B' || old[0] != 'A') {
        fail("stale file cache\n", 18);
    }
    sys2(215, (long)fresh, PAGE);
    sys2(215, (long)old, PAGE);
    sys1(57, tfd);
    sys3(35, AT_FDCWD, (long)"/tmp/mmapcache.dat", 0);

    sys3(64, 1, (long)"mmap cache ok\n", 14);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}