/* ICU file-mmap probe: maps /usr/lib/denial/flutter/data/icudtl.dat and
 * touches EVERY page, printing progress so a lazy-fill failure shows the
 * exact page. Freestanding (raw syscalls), like bigfile.c.
 *
 *   linuxrun /sfs/icuprobe
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

static void emit(const char *message, long length) {
    sys6(64, 1, (long)message, length, 0, 0, 0);
}

static void line_hex(const char *prefix, long prefix_length, unsigned value) {
    char buffer[32];
    for (long i = 0; i < prefix_length; i++) buffer[i] = prefix[i];
    long at = prefix_length;
    buffer[at++] = ' ';
    for (int shift = 28; shift >= 0; shift -= 4) {
        unsigned digit = (value >> shift) & 0xf;
        buffer[at++] = digit < 10 ? (char)('0' + digit) : (char)('a' + digit - 10);
    }
    buffer[at++] = '\n';
    emit(buffer, at);
}

void _start(void) {
    static const char path[] = "/usr/lib/denial/flutter/data/icudtl.dat";
    long fd = sys6(56, -100 /* AT_FDCWD */, (long)path, 0 /* O_RDONLY */, 0, 0, 0);
    line_hex("open", 4, (unsigned)fd);
    if (fd < 0) sys6(93, 1, 0, 0, 0, 0, 0);
    // size 10876560 = 0xA61110; round up to pages.
    static const char *const probes[] = {
        "/usr/lib/denial/flutter/lib/libapp.so",
        "/usr/lib/denial/flutter/lib/libflutter_engine.so",
        "/usr/lib/denial/flutter/data/icudtl.dat",
        "/usr/lib/denial/flutter/data/flutter_assets/version.json",
        0,
    };
    for (int i = 0; probes[i]; i++) {
        long pfd = sys6(56, -100, (long)probes[i], 0, 0, 0, 0);
        line_hex("pre", 3, (unsigned)pfd);
        const char *q = probes[i];
        while (*q) q++;
        emit(probes[i], q - probes[i]);
        emit("\n", 1);
    }
    long size = 10876560;
    long map = sys6(222, 0, size, 1, 2, fd, 0);
    line_hex("mmap", 4, (unsigned)map);
    if (map < 0 && map > -4096) sys6(93, 1, 0, 0, 0, 0, 0);
    volatile unsigned char *p = (volatile unsigned char *)map;
    unsigned sum = 0;
    long pages = (size + 4095) / 4096;
    for (long i = 0; i < pages; i++) {
        sum += p[i * 4096];
        if (i % 512 == 0) line_hex("page", 4, (unsigned)i);
    }
    line_hex("sum", 3, sum);
    for (int i = 0; probes[i]; i++) {
        long pfd = sys6(56, -100, (long)probes[i], 0, 0, 0, 0);
        line_hex("post", 4, (unsigned)pfd);
        const char *q = probes[i];
        while (*q) q++;
        emit(probes[i], q - probes[i]);
        emit("\n", 1);
    }

    line_hex("done", 4, 0x600d);
    sys6(93, 0, 0, 0, 0, 0, 0);
}
