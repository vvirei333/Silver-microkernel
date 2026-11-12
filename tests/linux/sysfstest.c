/* Synthetic sysfs smoke test: stat, read and readlink the DRM nodes. */
static long sys6(long n, long a, long b, long c, long d, long e, long f) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    register long x4 __asm__("x4") = e;
    register long x5 __asm__("x5") = f;
    __asm__ volatile("svc #0" : "+r"(x0)
                     : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5)
                     : "memory");
    return x0;
}
static long sys3(long n, long a, long b, long c) { return sys6(n, a, b, c, 0, 0, 0); }

static void print(const char *text, unsigned length) { sys3(64, 1, (long)text, length); }

static void print_number(const char *label, long value) {
    char digits[24];
    int count = 0;
    unsigned long number = value < 0 ? (unsigned long)-value : (unsigned long)value;
    unsigned length = 0;
    while (label[length] != 0) length++;
    print(label, length);
    if (value < 0) print("-", 1);
    do { digits[count++] = '0' + (number % 10); number /= 10; } while (number);
    char out[24];
    int index = 0;
    while (count) out[index++] = digits[--count];
    out[index++] = '\n';
    sys3(64, 1, (long)out, index);
}

void _start(void) {
    static const char vendor_path[] = "/sys/dev/char/226:128/device/vendor";
    static const char subsystem_path[] = "/sys/dev/char/226:128/device/subsystem";
    static const char uevent_path[] = "/sys/dev/char/226:128/device/uevent";
    static const char card_path[] = "/dev/dri/card0";

    char stat[128];
    print_number("stat card0 ", sys3(79, -100, (long)card_path, (long)stat));
    print_number("stat vendor ", sys3(79, -100, (long)vendor_path, (long)stat));

    char target[128];
    long link = sys3(78, -100, (long)subsystem_path, (long)target);
    print_number("readlink ", link);
    if (link > 0) { print("link: ", 6); print(target, (unsigned)link); print("\n", 1); }

    long fd = sys3(56, -100, (long)vendor_path, 0);
    print_number("open vendor ", fd);
    if (fd >= 0) {
        char buffer[64];
        long got = sys3(63, fd, (long)buffer, sizeof(buffer));
        print_number("read vendor ", got);
        if (got > 0) { print("vendor: ", 8); print(buffer, (unsigned)got); }
        sys3(57, fd, 0, 0);
    }

    fd = sys3(56, -100, (long)uevent_path, 0);
    print_number("open uevent ", fd);
    if (fd >= 0) {
        char buffer[256];
        long got = sys3(63, fd, (long)buffer, sizeof(buffer));
        print_number("read uevent ", got);
        if (got > 0) print(buffer, (unsigned)got);
        sys3(57, fd, 0, 0);
    }
    print("sysfs test done\n", 16);
    sys3(93, 0, 0, 0);
    for (;;) {}
}
