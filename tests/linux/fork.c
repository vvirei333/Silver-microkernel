/* fork + execve + wait4 test, freestanding (raw syscalls).
 *
 *   linuxrun /sfs/linux-fork
 *
 * The child execs /sfs/linux-hello (exit 42); the parent waits for it and
 * checks WEXITSTATUS(status) == 42. Expected: "hello from linux abi",
 * "parent saw child 42", then the parent exits 0.
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

static long sys4(long n, long a, long b, long c, long d) {
    return sys5(n, a, b, c, d, 0);
}

static long sys3(long n, long a, long b, long c) {
    return sys5(n, a, b, c, 0, 0);
}

void _start(void) {
    /* clone(flags = SIGCHLD, stack = 0, ...) with fork semantics */
    long pid = sys5(220, 17, 0, 0, 0, 0);

    if (pid == 0) {
        char path[] = "/sfs/linux-hello";
        char name[] = "linux-hello";
        char *argv[] = {name, 0};
        char *envp[] = {0};
        sys3(221, (long)path, (long)argv, (long)envp); /* execve */
        sys3(64, 1, (long)"child exec failed\n", 18);
        sys3(93, 1, 0, 0);
        for (;;) {
        }
    }
    if (pid < 0) {
        sys3(64, 1, (long)"fork failed\n", 12);
        sys3(93, 2, 0, 0);
        for (;;) {
        }
    }

    int status = 0;
    long waited = sys4(260, pid, (long)&status, 0, 0); /* wait4 */
    if (waited != pid) {
        sys3(64, 1, (long)"wait failed\n", 12);
        sys3(93, 3, 0, 0);
        for (;;) {
        }
    }
    /* WIFEXITED && WEXITSTATUS == 42 */
    if (((status & 0x7f) != 0) || (((status >> 8) & 0xff) != 42)) {
        sys3(64, 1, (long)"bad child status\n", 17);
        sys3(93, 4, 0, 0);
        for (;;) {
        }
    }
    sys3(64, 1, (long)"parent saw child 42\n", 20);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
