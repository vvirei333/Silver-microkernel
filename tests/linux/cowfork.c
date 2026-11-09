/* Copy-on-write fork isolation.
 *
 *   linuxrun /sfs/linux-cowfork
 *
 * Expected: "cow fork ok" and exit 0.
 *
 * The parent fills a large BSS array, an anonymous mapping and a stack array,
 * forks, then waits. The child checks the inherited values, writes different
 * bytes everywhere and exits. The parent re-checks that its pages were not
 * changed, then writes its own values. This covers image data, mmap and stack
 * COW runs.
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
    if (image_data[0] != 0x55 || mapping[0] != 0x66 || stack_data[0] != 0x99 ||
        heap_data[0] != 0xBB || heap_data[1] != 0xCC) {
        fail("parent write failed\n", 20, 6);
    }

    sys3(64, 1, (long)"cow fork ok\n", 12);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
