/* Copy-on-write fork isolation.
 *
 *   linuxrun /sfs/linux-cowfork
 *
 * Expected: "cow fork ok" and exit 0.
 *
 * The parent fills a large BSS array, an anonymous mapping and a stack array,
 * forks, then waits. The child checks the inherited values, writes different
 * bytes everywhere and exits. The parent re-checks that its pages were not
 * changed, then writes its own values. It forks again after those writes so
 * the child must inherit the parent's private image, mmap, stack and heap
 * pages. This covers page-granular image, stack and anonymous mmap COW.
 * A live sleeping child also checks that re-granting write through mprotect
 * on a non-lazy memfd mapping does not bypass the COW copy.
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

static long sys5(long n, long a, long b, long c, long d, long e) {
    return sys6(n, a, b, c, d, e, 0);
}

static long sys4(long n, long a, long b, long c, long d) {
    return sys5(n, a, b, c, d, 0);
}

static long sys3(long n, long a, long b, long c) {
    return sys5(n, a, b, c, 0, 0);
}

static long sys2(long n, long a, long b) { return sys5(n, a, b, 0, 0, 0); }

static long sys1(long n, long a) { return sys5(n, a, 0, 0, 0, 0); }

static int fail(const char *message, long length, long code) {
    sys3(64, 1, (long)message, length);
    sys3(93, code, 0, 0);
    return 1;
}

static unsigned char image_data[1024 * 1024];
static unsigned char *mapping;
static unsigned char *stack_data;
static unsigned char *heap_data;
static unsigned char *cow_map;

/* After the parent has broken one page of each kind, a second fork must
 * inherit those private pages (not the stale base frames) and share them
 * copy-on-write in turn. */
static int child_check_after_parent(void) {
    if (image_data[0] != 0x55 || image_data[512 * 1024] != 0xA5) {
        return 20;
    }
    if (mapping[0] != 0x66 || mapping[128 * 1024] != 0x11 ||
        mapping[200 * 1024] != 0x77) {
        return 21;
    }
    if (stack_data[0] != 0x99 || heap_data[0] != 0xBB || heap_data[1] != 0xCC) {
        return 22;
    }
    image_data[0] = 0x15;
    image_data[512 * 1024] = 0x25;
    mapping[0] = 0x35;
    mapping[200 * 1024] = 0x27;
    stack_data[0] = 0x45;
    heap_data[0] = 0x55;
    if (image_data[0] != 0x15 || image_data[512 * 1024] != 0x25 ||
        mapping[0] != 0x35 || mapping[200 * 1024] != 0x27 ||
        stack_data[0] != 0x45 || heap_data[0] != 0x55) {
        return 23;
    }
    return 0;
}

static int child_check(void) {
    const long SIZE = 256 * 1024;
    if (image_data[0] != 0xA5 || image_data[512 * 1024] != 0xA5) {
        return 10;
    }
    if (mapping[0] != 0x11 || mapping[128 * 1024] != 0x11) {
        return 11;
    }
    if (stack_data[0] != 0x77) {
        return 12;
    }
    if (heap_data[0] != 0x99) {
        return 14;
    }
    image_data[0] = 0x11;
    image_data[512 * 1024] = 0x22;
    mapping[0] = 0x33;
    mapping[128 * 1024] = 0x44;
    stack_data[0] = 0x88;
    heap_data[0] = 0xAA;
    if (image_data[0] != 0x11 || mapping[0] != 0x33 || stack_data[0] != 0x88 ||
        heap_data[0] != 0xAA) {
        return 13;
    }
    (void)SIZE;
    return 0;
}

void _start(void) {
    const long SIZE = 256 * 1024;
    unsigned char local_stack[4096];
    stack_data = local_stack;
    mapping = (unsigned char *)sys6(
        222, 0, SIZE, 1 | 2 /* READ|WRITE */, 0x22 /* PRIVATE|ANON */, -1, 0);
    if ((long)mapping < 0) {
        fail("mmap failed\n", 11, 1);
    }
    for (long index = 0; index < SIZE; index += 4096) {
        mapping[index] = 0x11;
    }
    for (long index = 0; index < (long)sizeof(image_data); index += 4096) {
        image_data[index] = 0xA5;
    }
    image_data[0] = 0xA5;
    image_data[512 * 1024] = 0xA5;
    stack_data[0] = 0x77;
    long heap = sys1(214, 0); /* brk(0) */
    if (sys1(214, heap + 65536) < heap + 65536) {
        fail("brk failed\n", 10, 7);
    }
    heap_data = (unsigned char *)heap;
    heap_data[0] = 0x99;

    /* The anonymous mapping above is lazy, so fork gives the child private
     * copies on first touch. Use a non-lazy memfd-backed mapping to exercise
     * page-granular COW frames, then re-grant write with mprotect while the
     * child is alive. The parent must still copy, not write the shared page. */
    long mpfd = sys3(279, (long)"cowmp", 0, 0); /* memfd_create */
    if (mpfd < 0 || sys3(46, mpfd, SIZE, 0) < 0) {
        fail("memfd setup failed\n", 20, 28);
    }
    cow_map = (unsigned char *)sys6(
        222, 0, SIZE, 1 | 2, 0x2 /* PRIVATE */, mpfd, 0);
    if ((long)cow_map < 0) {
        fail("memfd mmap failed\n", 18, 29);
    }
    for (long index = 0; index < SIZE; index += 4096) {
        cow_map[index] = 0x11;
    }
    long delay[2] = {0, 200000000};
    long sync_pid = sys5(220, 17, 0, 0, 0, 0);
    if (sync_pid < 0) {
        fail("mprotect fork failed\n", 21, 23);
    }
    if (sync_pid == 0) {
        sys3(101, (long)delay, 0, 0);
        if (cow_map[64 * 1024] != 0x11) {
            sys3(93, 32, 0, 0);
        }
        sys3(93, 0, 0, 0);
        for (;;) {
        }
    }
    if (sys3(226, (long)cow_map, SIZE, 3) != 0) {
        fail("mprotect failed\n", 16, 24);
    }
    cow_map[64 * 1024] = 0x66;
    int sync_status = 0;
    if (sys4(260, sync_pid, (long)&sync_status, 0, 0) != sync_pid) {
        fail("mprotect wait failed\n", 22, 26);
    }
    if (((sync_status >> 8) & 0xff) != 0) {
        fail("mprotect child saw shared page\n", 31, 27);
    }
    if (cow_map[64 * 1024] != 0x66) {
        fail("mprotect page lost\n", 19, 30);
    }
    if (sys3(215, (long)cow_map, SIZE, 0) != 0) {
        fail("mprotect munmap failed\n", 24, 31);
    }

    long pid = sys5(220, 17, 0, 0, 0, 0);
    if (pid < 0) {
        fail("fork failed\n", 11, 2);
    }
    if (pid == 0) {
        sys3(93, child_check(), 0, 0);
        for (;;) {
        }
    }
    if (heap_data == 0) {
        fail("heap pointer zero after fork\n", 27, 8);
    }

    int status = 0;
    if (sys4(260, pid, (long)&status, 0, 0) != pid) {
        fail("wait failed\n", 11, 3);
    }
    if (((status >> 8) & 0xff) != 0) {
        fail("child saw shared page\n", 22, 4);
    }
    if (heap_data == 0) {
        fail("heap pointer zero after wait\n", 27, 9);
    }

    /* Child wrote all three kinds of pages; the parent must still see its own. */
    if (image_data[0] != 0xA5 || image_data[512 * 1024] != 0xA5 ||
        mapping[0] != 0x11 || mapping[128 * 1024] != 0x11 ||
        stack_data[0] != 0x77 || heap_data[0] != 0x99) {
        fail("parent page changed\n", 20, 5);
    }
    image_data[0] = 0x55;
    if (heap_data == 0) {
        fail("heap ptr lost after image write\n", 31, 10);
    }
    mapping[0] = 0x66;
    mapping[200 * 1024] = 0x77;
    if (heap_data == 0) {
        fail("heap ptr lost after mmap write\n", 31, 11);
    }
    stack_data[0] = 0x99;
    if (heap_data == 0) {
        fail("heap ptr lost after stack write\n", 31, 12);
    }
    register unsigned long preserved asm("x11") = 0x12345678UL;
    asm volatile("" : "+r"(preserved));
    heap_data[1] = 0xCC;
    if (preserved != 0x12345678UL) {
        fail("x11 not preserved across COW\n", 28, 13);
    }
    heap_data[0] = 0xBB;
    if (image_data[0] != 0x55 || mapping[0] != 0x66 ||
        mapping[200 * 1024] != 0x77 || stack_data[0] != 0x99 ||
        heap_data[0] != 0xBB || heap_data[1] != 0xCC) {
        fail("parent write failed\n", 20, 6);
    }

    /* The parent has now broken image, mmap, stack and heap pages. A second
     * child must see those private values, not the original base run. */
    long pid2 = sys5(220, 17, 0, 0, 0, 0);
    if (pid2 < 0) {
        fail("second fork failed\n", 18, 14);
    }
    if (pid2 == 0) {
        sys3(93, child_check_after_parent(), 0, 0);
        for (;;) {
        }
    }
    int status2 = 0;
    if (sys4(260, pid2, (long)&status2, 0, 0) != pid2) {
        fail("second wait failed\n", 18, 15);
    }
    if (((status2 >> 8) & 0xff) != 0) {
        fail("second child saw stale page\n", 27, 16);
    }
    if (image_data[0] != 0x55 || image_data[512 * 1024] != 0xA5 ||
        mapping[0] != 0x66 || mapping[200 * 1024] != 0x77 ||
        stack_data[0] != 0x99 || heap_data[0] != 0xBB) {
        fail("second child changed parent\n", 27, 17);
    }
    /* The region has three private frames. Unmap a prefix, a middle slice
     * and the tail to exercise offset shifting and slice release. */
    if (sys2(215, (long)mapping, 64 * 1024) != 0) {
        fail("munmap prefix failed\n", 21, 18);
    }
    if (sys2(215, (long)mapping + 128 * 1024, 64 * 1024) != 0) {
        fail("munmap middle failed\n", 20, 19);
    }
    if (sys2(215, (long)mapping + 192 * 1024, 64 * 1024) != 0) {
        fail("munmap tail failed\n", 18, 20);
    }
    /* Only the [64K,128K) slice is left. It was populated in the shared
     * lazy object, and the two unmaps above shifted the region's base bias.
     * A write must still find and copy the original object page. */
    mapping[64 * 1024] = 0x7A;
    if (mapping[64 * 1024] != 0x7A) {
        fail("shifted lazy page lost\n", 23, 22);
    }
    if (sys2(215, (long)mapping + 64 * 1024, 64 * 1024) != 0) {
        fail("munmap rest failed\n", 18, 21);
    }

    sys3(64, 1, (long)"cow fork ok\n", 12);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
