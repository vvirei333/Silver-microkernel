/* PRIME/GEM export test: a render-node BO gets a GEM name, an exported fd,
 * and the pixels are visible both through a second mapping and in a child
 * process that inherits the fd. */
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

struct resource_create {
    unsigned target, format, bind, width, height, depth, array_size, last_level;
    unsigned nr_samples, flags, bo_handle, res_handle, size, stride;
};
struct virtgpu_map { unsigned long offset; unsigned handle; unsigned pad; };
struct gem_flink { unsigned handle, name; };
struct gem_open { unsigned name, handle; unsigned long size; };
struct prime_handle { unsigned handle, flags; int fd; };

#define DRM_IOCTL_VIRTGPU_RESOURCE_CREATE 0xC0386444ul
#define DRM_IOCTL_VIRTGPU_MAP 0xC0106441ul
#define DRM_IOCTL_GEM_FLINK 0xC008640Aul
#define DRM_IOCTL_GEM_OPEN 0xC010640Bul
#define DRM_IOCTL_PRIME_HANDLE_TO_FD 0xC00C642Dul

static unsigned pattern(unsigned index) { return 0x5A5A0000u | (index & 0xFFFFu); }

void _start(void) {
    long fd = sys3(56, -100, (long)"/dev/dri/renderD128", 2);
    if (fd < 0) {
        print("prime: no renderD128\n", 21);
        sys3(93, 1, 0, 0);
    }
    struct resource_create create;
    for (unsigned i = 0; i < sizeof(create); i++) ((char *)&create)[i] = 0;
    create.target = 2;
    create.format = 1;
    create.bind = 2;
    create.width = 64;
    create.height = 64;
    create.depth = 1;
    create.array_size = 1;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, (long)&create) < 0) {
        print("prime: resource create failed\n", 29);
        sys3(93, 1, 0, 0);
    }
    struct virtgpu_map map;
    map.offset = 0; map.handle = create.bo_handle; map.pad = 0;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_MAP, (long)&map) < 0) {
        print("prime: MAP failed\n", 16);
        sys3(93, 1, 0, 0);
    }
    long mapped = sys6(222, 0, create.size, 3, 1, fd, (long)map.offset);
    if (mapped <= 0) {
        print("prime: mmap failed\n", 18);
        sys3(93, 1, 0, 0);
    }
    unsigned *pixels = (unsigned *)mapped;
    for (unsigned i = 0; i < 64; i++) pixels[i] = pattern(i);

    struct gem_flink link;
    link.handle = create.bo_handle; link.name = 0;
    if (sys3(29, fd, DRM_IOCTL_GEM_FLINK, (long)&link) < 0) {
        print("prime: FLINK failed\n", 19);
        sys3(93, 1, 0, 0);
    }
    print_number("prime: gem name ", link.name);

    struct gem_open open;
    open.name = link.name; open.handle = 0; open.size = 0;
    if (sys3(29, fd, DRM_IOCTL_GEM_OPEN, (long)&open) < 0) {
        print("prime: GEM_OPEN failed\n", 22);
        sys3(93, 1, 0, 0);
    }
    print_number("prime: reopened handle ", open.handle);

    struct prime_handle prime;
    prime.handle = create.bo_handle; prime.flags = 2 /* write */; prime.fd = -1;
    if (sys3(29, fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, (long)&prime) < 0) {
        print("prime: HANDLE_TO_FD failed\n", 26);
        sys3(93, 1, 0, 0);
    }
    print_number("prime: exported fd ", prime.fd);

    long shared = sys6(222, 0, create.size, 1 /* read */, 1 /* shared */,
                       prime.fd, 0);
    if (shared <= 0) {
        print("prime: shared mmap failed\n", 25);
        sys3(93, 1, 0, 0);
    }
    const unsigned *seen = (const unsigned *)shared;
    int ok = 1;
    for (unsigned i = 0; i < 64; i++) {
        if (seen[i] != pattern(i)) ok = 0;
    }
    print("prime: shared mapping matches\n", 29);

    long child = sys6(220 /* clone */, 17, 0, 0, 0, 0, 0);
    if (child == 0) {
        long child_map = sys6(222, 0, create.size, 1, 1, prime.fd, 0);
        if (child_map <= 0) {
            print("prime: child mmap failed\n", 24);
            sys3(93, 1, 0, 0);
        }
        const unsigned *child_seen = (const unsigned *)child_map;
        for (unsigned i = 0; i < 64; i++) {
            if (child_seen[i] != pattern(i)) {
                print("prime: child mismatch\n", 21);
                sys3(93, 1, 0, 0);
            }
        }
        print("prime: child sees the pattern\n", 30);
        sys3(93, 0, 0, 0);
        for (;;) {}
    }
    sys6(61 /* wait4 */, child, 0, 0, 0, 0, 0);
    print(ok ? "prime test ok\n" : "prime test failed\n", ok ? 14 : 18);
    sys3(93, 0, 0, 0);
    for (;;) {}
}
