/* Freestanding AArch64 Linux program used by the Linux ABI self-test.
 *
 * It makes raw Linux syscalls (`svc #0` with x8 = number) and links at the
 * address a normal Linux binary uses:
 *
 *   linuxrun /sfs/linux-hello
 *
 * Expected console output: "hello from linux abi" and exit status 42.
 */
static long sys3(long n, long a, long b, long c) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}

static long sys1(long n, long a) { return sys3(n, a, 0, 0); }

void _start(void) {
    static const char msg[] = "hello from linux abi\n";
    sys3(64, 1, (long)msg, sizeof(msg) - 1); /* write(1, msg, len) */
    sys1(93, 42);                           /* exit(42) */
    for (;;) {
    }
}
