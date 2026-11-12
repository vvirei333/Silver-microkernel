/* virtgpu render-node smoke test: query 3D features, fetch the virgl caps,
 * create a context and a 3D resource, map it and submit a command buffer. */
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
static long sys3(long n, long a, long b, long c) { return sys6(n, a, b, c, 0, 0, 0); }

static void print(const char *text, unsigned length) {
    sys3(64, 1, (long)text, length);
}

static void print_number(const char *label, long value) {
    char digits[24];
    int count = 0;
    unsigned long number = value < 0 ? (unsigned long)-value : (unsigned long)value;
    unsigned label_length = 0;
    while (label[label_length] != 0) {
        label_length += 1;
    }
    print(label, label_length);
    if (value < 0) {
        print("-", 1);
    }
    do {
        digits[count++] = '0' + (number % 10);
        number /= 10;
    } while (number != 0);
    char out[24];
    int index = 0;
    while (count != 0) {
        out[index++] = digits[--count];
    }
    out[index++] = '\n';
    sys3(64, 1, (long)out, index);
}

struct getparam {
    unsigned long param;
    unsigned long value;
};

struct get_caps {
    unsigned cap_set_id;
    unsigned cap_set_ver;
    unsigned long addr;
    unsigned size;
    unsigned pad;
};

struct context_init {
    unsigned num_params;
    unsigned pad;
    unsigned long ctx_set_params;
};

struct resource_create {
    unsigned target, format, bind, width, height, depth, array_size, last_level;
    unsigned nr_samples, flags, bo_handle, res_handle, size, stride;
};

struct virtgpu_map {
    unsigned long offset;
    unsigned handle;
    unsigned pad;
};

struct virtgpu_execbuffer {
    unsigned flags;
    unsigned size;
    unsigned long command;
    unsigned long bo_handles;
    unsigned num_bo_handles;
    int fence_fd;
    unsigned ring_idx;
    unsigned syncobj_stride;
    unsigned num_in_syncobjs;
    unsigned num_out_syncobjs;
    unsigned long in_syncobjs;
    unsigned long out_syncobjs;
};

#define DRM_IOCTL_VIRTGPU_MAP 0xC0106441ul
#define DRM_IOCTL_VIRTGPU_EXECBUFFER 0xC0406442ul
#define DRM_IOCTL_VIRTGPU_GETPARAM 0xC0106443ul
#define DRM_IOCTL_VIRTGPU_RESOURCE_CREATE 0xC0386444ul
#define DRM_IOCTL_VIRTGPU_GET_CAPS 0xC0186449ul
#define DRM_IOCTL_VIRTGPU_CONTEXT_INIT 0xC010644Bul

/* VIRGL_CCMD_NOP: a zero dword is a command with opcode 0 and no payload. */
static unsigned commands[1];

void _start(void) {
    long fd = sys3(56 /* openat */, -100, (long)"/dev/dri/renderD128", 2 /* O_RDWR */);
    if (fd < 0) {
        print("render: no renderD128\n", 22);
        sys3(93, 1, 0, 0);
    }
    print("render: renderD128 open\n", 23);

    /* `value` is a user pointer to the result, as in the real ioctl. */
    struct getparam param;
    unsigned long param_value = 0;
    param.param = 1; /* VIRTGPU_PARAM_3D_FEATURES */
    param.value = (unsigned long)&param_value;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_GETPARAM, (long)&param) < 0) {
        print("render: GETPARAM failed\n", 23);
        sys3(93, 1, 0, 0);
    }
    print_number("render: 3d features ", (long)param_value);

    param_value = 0;
    param.param = 7; /* VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS */
    param.value = (unsigned long)&param_value;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_GETPARAM, (long)&param) < 0) {
        print("render: capset ids failed\n", 26);
        sys3(93, 1, 0, 0);
    }
    print_number("render: capset ids ", (long)param_value);

    static unsigned char caps[4096];
    struct get_caps getcaps;
    getcaps.cap_set_id = 2; /* VIRTGPU_DRM_CAPSET_VIRGL2 */
    getcaps.cap_set_ver = 2;
    getcaps.addr = (unsigned long)caps;
    getcaps.size = sizeof(caps);
    getcaps.pad = 0;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_GET_CAPS, (long)&getcaps) < 0) {
        print("render: GET_CAPS failed\n", 23);
        sys3(93, 1, 0, 0);
    }
    print_number("render: caps bytes ", getcaps.size);

    struct context_init context;
    context.num_params = 0;
    context.pad = 0;
    context.ctx_set_params = 0;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, (long)&context) < 0) {
        print("render: CONTEXT_INIT failed\n", 27);
        sys3(93, 1, 0, 0);
    }
    print("render: context ok\n", 19);

    struct resource_create create;
    for (unsigned i = 0; i < sizeof(create); i++) {
        ((char *)&create)[i] = 0;
    }
    create.target = 2;      /* VIRGL_TARGET_TEXTURE_2D */
    create.format = 1;      /* VIRGL_FORMAT_B8G8R8A8_UNORM */
    create.bind = 2 | 8;    /* RENDER_TARGET | SAMPLER_VIEW */
    create.width = 64;
    create.height = 64;
    create.depth = 1;
    create.array_size = 1;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, (long)&create) < 0) {
        print("render: RESOURCE_CREATE failed\n", 30);
        sys3(93, 1, 0, 0);
    }
    print_number("render: bo handle ", create.bo_handle);
    print_number("render: res handle ", create.res_handle);
    print_number("render: size ", create.size);

    struct virtgpu_map map;
    map.offset = 0;
    map.handle = create.bo_handle;
    map.pad = 0;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_MAP, (long)&map) < 0) {
        print("render: MAP failed\n", 17);
        sys3(93, 1, 0, 0);
    }
    long mapped = sys6(222 /* mmap */, 0, create.size, 3 /* RW */, 1 /* SHARED */,
                       fd, (long)map.offset);
    if (mapped <= 0) {
        print("render: mmap failed\n", 19);
        sys3(93, 1, 0, 0);
    }
    print("render: bo mapped\n", 18);

    commands[0] = 0; /* VIRGL_CCMD_NOP */
    struct virtgpu_execbuffer exec;
    for (unsigned i = 0; i < sizeof(exec); i++) {
        ((char *)&exec)[i] = 0;
    }
    exec.flags = 2; /* VIRTGPU_EXECBUF_FENCE_FD_OUT */
    exec.size = sizeof(commands);
    exec.command = (unsigned long)commands;
    exec.fence_fd = -1;
    if (sys3(29, fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, (long)&exec) < 0) {
        print("render: EXECBUFFER failed\n", 24);
        sys3(93, 1, 0, 0);
    }
    print_number("render: fence fd ", exec.fence_fd);
    if (exec.fence_fd < 0) {
        print("render: no fence fd\n", 21);
        sys3(93, 1, 0, 0);
    }
    print("render test ok\n", 16);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
