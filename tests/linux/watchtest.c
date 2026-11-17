/* Watchpoint self-test: map a fixed page and store into it.
 *
 *   watch 0x50000000
 *   linuxrun /sfs/watchtest
 *
 * The kernel build must include `gdb-stub`; the store traps into the GDB stub
 * on the second PL011 instead of completing, so this program never prints.
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

static long sys1(long n, long a) { return sys6(n, a, 0, 0, 0, 0, 0); }

void _start(void) {
    const long MAP_FIXED_NOREPLACE = 0x100000;
    const long MAP_PRIVATE = 0x02;
    const long MAP_ANONYMOUS = 0x20;
    long mapped = sys6(222, 0x50000000, 4096, 3,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (mapped != 0x50000000) {
        sys3(64, 1, (long)"watch map failed\n", 18);
        sys1(93, 1);
        for (;;) {
        }
    }
    *(volatile unsigned long *)0x50000000 = 0x12345678;
    sys3(64, 1, (long)"watch survived\n", 15);
    sys1(93, 0);
    for (;;) {
    }
}
