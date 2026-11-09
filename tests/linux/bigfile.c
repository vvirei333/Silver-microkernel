/* Large-file read: proves the enlarged rootfs is mounted and usable.
 *
 *   linuxrun /sfs/linux-bigfile
 *
 * Reads /sfs/big.bin (3 MiB, byte i is `i & 0xff`) in 64 KiB chunks, checks
 * every byte and prints "rootfs ok". A kernel that still reads only the first
 * MiB of the disk image cannot mount the 16 MiB SFS at all, and the old
 * 512 KiB VFS/exec limits would reject this file.
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

static long sys2(long n, long a, long b) { return sys5(n, a, b, 0, 0, 0); }

static long sys1(long n, long a) { return sys5(n, a, 0, 0, 0, 0); }

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

static void write_hex(unsigned long value) {
    char text[18];
    text[0] = '0';
    text[1] = 'x';
    for (int index = 0; index < 16; index++) {
        unsigned long digit = (value >> (60 - index * 4)) & 0xf;
        text[2 + index] = (char)(digit < 10 ? '0' + digit : 'a' + digit - 10);
    }
    sys3(64, 1, (long)text, 18);
}

static int fail_at(const char *message, long length, long position, long got) {
    sys3(64, 1, (long)message, length);
    sys3(64, 1, (long)" pos=", 5);
    write_hex((unsigned long)position);
    sys3(64, 1, (long)" got=", 5);
    write_hex((unsigned long)got);
    sys3(64, 1, (long)"\n", 1);
    sys3(93, 1, 0, 0);
    return 1;
}

static char buffer[64 * 1024];

void _start(void) {
    const long AT_FDCWD = -100;
    long fd = sys4(56, AT_FDCWD, (long)"/sfs/big.bin", 0, 0);
    if (fd < 0) {
        fail("open big.bin failed\n", 20);
    }

    unsigned char stat[128];
    if (sys2(80, fd, (long)stat) != 0) {
        fail("fstat big.bin failed\n", 21);
    }
    long size = 0;
    for (int index = 0; index < 8; index++) {
        size |= (long)stat[48 + index] << (index * 8);
    }
    if (size != 3 * 1024 * 1024) {
        fail("big.bin size wrong\n", 18);
    }

    long position = 0;
    while (position < size) {
        long want = size - position;
        if (want > (long)sizeof(buffer)) {
            want = sizeof(buffer);
        }
        long got = sys3(63, fd, (long)buffer, want);
        if (got <= 0) {
            fail_at("read big.bin failed", 19, position, got);
        }
        for (long index = 0; index < got; index++) {
            if ((unsigned char)buffer[index] != (unsigned char)(position + index)) {
                fail_at("big.bin content wrong", 21, position + index, got);
            }
        }
        position += got;
    }
    sys1(57, fd);

    sys3(64, 1, (long)"rootfs ok\n", 10);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
