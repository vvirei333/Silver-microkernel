/* execve test: a freestanding program that replaces itself with
 * /sfs/linux-hello.
 *
 *   linuxrun /sfs/linux-chain
 *
 * Expected: "hello from linux abi" and exit status 42 (from the target).
 * Everything lives on the stack, so one RX segment is enough.
 */
static long sys3(long n, long a, long b, long c) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}

void _start(void) {
    char path[] = "/sfs/linux-hello";
    char name[] = "linux-hello";
    char environment[] = "PATH=/sfs";
    char *argv[] = {name, 0};
    char *envp[] = {environment, 0};
    /* execve(2) = 221 on AArch64 */
    sys3(221, (long)path, (long)argv, (long)envp);
    /* Reached only if execve failed. */
    sys3(64, 1, (long)"execve failed\n", 14);
    sys3(93, 1, 0, 0);
    for (;;) {
    }
}
