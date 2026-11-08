/* Multi-segment test: proves the Linux loader maps text RX and data RW at the
 * file's own addresses, and that BSS (memsz > filesz) is zero-filled and
 * writable.
 *
 *   linuxrun /sfs/linux-rw
 *
 * Expected: "RW data+bss ok" and exit status 7.
 */
static char message[] = "seed\n";
static char scratch[8192];

static long sys3(long n, long a, long b, long c) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}

void _start(void) {
    /* BSS must read as zero and accept writes. */
    for (int i = 0; i < (int)sizeof(scratch); i++) {
        if (scratch[i] != 0) {
            sys3(64, 1, (long)"bss not zero\n", 13);
            sys3(93, 3, 0, 0);
        }
        scratch[i] = (char)i;
    }
    /* .data must be writable. */
    message[0] = 'R';
    message[1] = 'W';
    long sum = 0;
    for (int i = 0; i < (int)sizeof(scratch); i++) {
        if (scratch[i] != (char)i) {
            sys3(64, 1, (long)"bss lost\n", 9);
            sys3(93, 4, 0, 0);
        }
        sum += scratch[i];
    }
    if (sum == 0) {
        sys3(93, 5, 0, 0);
    }
    sys3(64, 1, (long)message, 5);
    sys3(64, 1, (long)" data+bss ok\n", 13);
    sys3(93, 7, 0, 0);
    for (;;) {
    }
}
