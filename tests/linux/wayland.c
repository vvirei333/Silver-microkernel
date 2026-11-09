/* A minimal Wayland wire-protocol exchange over the Linux ABI.
 *
 *   linuxrun /sfs/linux-wayland
 *
 * The parent is a tiny compositor: it listens on a named AF_UNIX socket,
 * answers wl_display.get_registry/sync, accepts the client's bind of
 * wl_compositor and wl_shm, receives the shm pool descriptor with
 * SCM_RIGHTS, maps it MAP_SHARED, and verifies the pixels the client drew
 * before acknowledging the final commit. The child is the client.
 *
 * Expected: "wayland ok" and exit 0.
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
static long sys3(long n, long a, long b, long c) { return sys6(n, a, b, c, 0, 0, 0); }

static int fail(const char *message, long length) {
    sys3(64, 1, (long)message, length);
    sys3(93, 1, 0, 0);
    return 1;
}

struct iovec {
    void *base;
    unsigned long length;
};
struct msghdr {
    void *name;
    unsigned int name_length;
    unsigned int pad;
    struct iovec *iov;
    unsigned long iov_length;
    void *control;
    unsigned long control_length;
    int flags;
    int pad2;
};
struct cmsghdr {
    unsigned long length;
    int level;
    int kind;
};
union control {
    struct cmsghdr header;
    unsigned char raw[64];
};
struct sockaddr_un {
    unsigned short family;
    char path[108];
};

static unsigned char message[1024];
static unsigned long message_length;
static unsigned char incoming[1024];
static unsigned long incoming_length;
static unsigned long incoming_offset;
static int incoming_fd;

static void put_u16(unsigned value) {
    message[message_length++] = value & 0xff;
    message[message_length++] = (value >> 8) & 0xff;
}
static void put_u32(unsigned value) {
    message[message_length++] = value & 0xff;
    message[message_length++] = (value >> 8) & 0xff;
    message[message_length++] = (value >> 16) & 0xff;
    message[message_length++] = (value >> 24) & 0xff;
}
static void put_i32(int value) { put_u32((unsigned)value); }
static void put_string(const char *text) {
    unsigned length = 0;
    while (text[length] != 0) {
        length++;
    }
    length++;
    put_u32(length);
    for (unsigned index = 0; index < length; index++) {
        message[message_length++] = (unsigned char)text[index];
    }
    while (message_length % 4 != 0) {
        message[message_length++] = 0;
    }
}
static void begin(unsigned object, unsigned opcode) {
    message_length = 0;
    put_u32(object);
    /* Wire order is object id, opcode, size (the size includes the header). */
    put_u16(opcode);
    put_u16(8);
}
static void finish(void) {
    while (message_length % 4 != 0) {
        message[message_length++] = 0;
    }
    message[6] = message_length & 0xff;
    message[7] = (message_length >> 8) & 0xff;
}

static long send_message(long fd, int pass_fd) {
    struct iovec vector = {message, message_length};
    struct msghdr header;
    union control control;
    for (unsigned long index = 0; index < sizeof(header); index++) {
        ((unsigned char *)&header)[index] = 0;
    }
    header.iov = &vector;
    header.iov_length = 1;
    if (pass_fd >= 0) {
        control.header.length = sizeof(struct cmsghdr) + sizeof(int);
        control.header.level = 1; /* SOL_SOCKET */
        control.header.kind = 1;  /* SCM_RIGHTS */
        *(int *)((unsigned char *)&control.header + sizeof(struct cmsghdr)) = pass_fd;
        header.control = control.raw;
        header.control_length = sizeof(struct cmsghdr) + sizeof(int);
    }
    return sys3(211 /* sendmsg */, fd, (long)&header, 0);
}

/* Reads one queued message; returns 0 on EOF. */
static long receive_message(long fd) {
    struct iovec vector = {incoming, sizeof(incoming)};
    struct msghdr header;
    union control control;
    for (unsigned long index = 0; index < sizeof(header); index++) {
        ((unsigned char *)&header)[index] = 0;
    }
    header.iov = &vector;
    header.iov_length = 1;
    header.control = control.raw;
    header.control_length = sizeof(control.raw);
    long count = sys3(212 /* recvmsg */, fd, (long)&header, 0);
    if (count <= 0) {
        return count;
    }
    incoming_length = (unsigned long)count;
    incoming_offset = 8;
    incoming_fd = -1;
    if (header.control_length >= sizeof(struct cmsghdr) + sizeof(int)) {
        struct cmsghdr *got = &control.header;
        if (got->level == 1 && got->kind == 1) {
            incoming_fd =
                *(int *)((unsigned char *)got + sizeof(struct cmsghdr));
        }
    }
    return count;
}

static unsigned take_u32(void) {
    unsigned value = incoming[incoming_offset] |
                     (incoming[incoming_offset + 1] << 8) |
                     (incoming[incoming_offset + 2] << 16) |
                     (incoming[incoming_offset + 3] << 24);
    incoming_offset += 4;
    return value;
}
static int take_i32(void) { return (int)take_u32(); }
static void take_string(char *output, unsigned capacity) {
    unsigned length = take_u32();
    unsigned copied = 0;
    while (copied + 1 < capacity && copied < length) {
        output[copied] = (char)incoming[incoming_offset + copied];
        copied++;
    }
    output[copied] = 0;
    incoming_offset += (length + 3) & ~3u;
}

static unsigned object_of(void) {
    return incoming[0] | (incoming[1] << 8) | (incoming[2] << 16) |
           (incoming[3] << 24);
}
static unsigned opcode_of(void) { return incoming[4] | (incoming[5] << 8); }

static int text_equal(const char *left, const char *right) {
    while (*left != 0 && *left == *right) {
        left++;
        right++;
    }
    return *left == *right;
}

static const char NAME[] = "/tmp/wayland-test";
static const unsigned ARGB8888 = 0;

static long bind_path(long socket) {
    struct sockaddr_un address;
    for (unsigned long index = 0; index < sizeof(address.path); index++) {
        address.path[index] = 0;
    }
    address.family = 1;
    unsigned length = 0;
    while (NAME[length] != 0) {
        address.path[length] = NAME[length];
        length++;
    }
    return sys3(200 /* bind */, socket, (long)&address,
                sizeof(unsigned short) + length + 1);
}

static long connect_path(long socket) {
    struct sockaddr_un address;
    for (unsigned long index = 0; index < sizeof(address.path); index++) {
        address.path[index] = 0;
    }
    address.family = 1;
    unsigned length = 0;
    while (NAME[length] != 0) {
        address.path[length] = NAME[length];
        length++;
    }
    return sys3(203 /* connect */, socket, (long)&address,
                sizeof(unsigned short) + length + 1);
}

/* ------------------------------------------------------------------ server */

static unsigned compositor_id;
static unsigned shm_id;
static unsigned surface_id;
static unsigned pool_id;
static unsigned buffer_id;
static int pool_fd = -1;
static int verified;

static void send_global(long client, unsigned registry, unsigned name,
                        const char *interface) {
    begin(registry, 0 /* wl_registry.global */);
    put_u32(name);
    put_string(interface);
    put_u32(1);
    finish();
    send_message(client, -1);
}

static void send_done(long client, unsigned callback, unsigned data) {
    begin(callback, 0 /* wl_callback.done */);
    put_u32(data);
    finish();
    send_message(client, -1);
}

static int serve(long client) {
    for (;;) {
        long count = receive_message(client);
        if (count == 0) {
            return verified;
        }
        if (count < 0) {
            return 0;
        }
        if (incoming_length < 8) {
            continue;
        }
        unsigned object = object_of();
        unsigned opcode = opcode_of();

        if (object == 1 && opcode == 0) { /* wl_display.sync */
            unsigned callback = take_u32();
            send_done(client, callback, 0);
        } else if (object == 1 && opcode == 1) { /* wl_display.get_registry */
            unsigned registry = take_u32();
            send_global(client, registry, 1, "wl_compositor");
            send_global(client, registry, 2, "wl_shm");
        } else if (object == 2 && opcode == 0) { /* wl_registry.bind */
            (void)take_u32(); /* name */
            char interface[64];
            take_string(interface, sizeof(interface));
            (void)take_u32(); /* version */
            unsigned new_id = take_u32();
            if (text_equal(interface, "wl_compositor")) {
                compositor_id = new_id;
            } else if (text_equal(interface, "wl_shm")) {
                shm_id = new_id;
            }
        } else if (object == 3 && object == compositor_id && opcode == 0) {
            surface_id = take_u32(); /* wl_compositor.create_surface */
        } else if (object == shm_id && opcode == 0) {
            /* wl_shm.create_pool(new_id, fd, size) */
            pool_id = take_u32();
            int size = take_i32();
            (void)size;
            if (incoming_fd >= 0) {
                if (pool_fd >= 0) {
                    sys3(57, pool_fd, 0, 0);
                }
                pool_fd = incoming_fd;
            }
        } else if (object == pool_id && opcode == 0) {
            /* wl_shm_pool.create_buffer(new_id, offset, w, h, stride, format) */
            buffer_id = take_u32();
            (void)take_i32(); /* offset */
            (void)take_i32(); /* width */
            (void)take_i32(); /* height */
            (void)take_i32(); /* stride */
            (void)take_u32(); /* format */
        } else if (object == surface_id && opcode == 6) {
            /* wl_surface.commit */
            if (pool_fd >= 0 && buffer_id != 0) {
                long mapped = sys6(222 /* mmap */, 0, 4096, 1 /* READ */,
                                   1 /* MAP_SHARED */, pool_fd, 0);
                if (mapped > 0) {
                    unsigned *pixels = (unsigned *)mapped;
                    if (pixels[0] == 0xFF112233u &&
                        pixels[1] == 0xFF445566u) {
                        verified = 1;
                    }
                    sys3(215 /* munmap */, mapped, 4096, 0);
                }
            }
        } else if (object == 1 && opcode >= 2) {
            /* Unknown display request: ignore for forward compatibility. */
        }
    }
}

/* ------------------------------------------------------------------ client */

static unsigned global_compositor;
static unsigned global_shm;

static void client_bind(long display) {
    begin(2, 0 /* wl_registry.bind */);
    put_u32(global_compositor);
    put_string("wl_compositor");
    put_u32(1);
    put_u32(3);
    finish();
    send_message(display, -1);

    begin(2, 0);
    put_u32(global_shm);
    put_string("wl_shm");
    put_u32(1);
    put_u32(4);
    finish();
    send_message(display, -1);
}

static int client_roundtrip(long display, unsigned callback) {
    begin(1, 0 /* wl_display.sync */);
    put_u32(callback);
    finish();
    if (send_message(display, -1) <= 0) {
        return 0;
    }
    for (;;) {
        if (receive_message(display) <= 0) {
            return 0;
        }
        if (object_of() == callback && opcode_of() == 0) {
            return 1;
        }
    }
}

static int run_client(void) {
    long display = sys3(198 /* socket */, 1, 1, 0);
    if (display < 0 || connect_path(display) < 0) {
        return fail("wayland: connect failed\n", 24);
    }

    begin(1, 1 /* wl_display.get_registry */);
    put_u32(2);
    finish();
    if (send_message(display, -1) <= 0) {
        return fail("wayland: get_registry failed\n", 28);
    }
    while (global_compositor == 0 || global_shm == 0) {
        if (receive_message(display) <= 0) {
            return fail("wayland: no globals\n", 21);
        }
        if (object_of() == 2 && opcode_of() == 0) {
            unsigned name = take_u32();
            char interface[64];
            take_string(interface, sizeof(interface));
            (void)take_u32();
            if (text_equal(interface, "wl_compositor")) {
                global_compositor = name;
            } else if (text_equal(interface, "wl_shm")) {
                global_shm = name;
            }
        }
    }
    client_bind(display);
    if (!client_roundtrip(display, 5)) {
        return fail("wayland: first sync failed\n", 26);
    }

    /* wl_compositor.create_surface */
    begin(3, 0);
    put_u32(6);
    finish();
    send_message(display, -1);

    /* A shm pool backed by a memfd, with two known ARGB pixels. */
    long pool = sys3(279 /* memfd_create */, (long)"wayland", 0, 0);
    if (pool < 0 || sys3(46 /* ftruncate */, pool, 4096, 0) < 0) {
        return fail("wayland: memfd failed\n", 22);
    }
    long pixels = sys6(222 /* mmap */, 0, 4096, 3 /* RW */, 1 /* SHARED */,
                       pool, 0);
    if (pixels < 0) {
        return fail("wayland: mmap failed\n", 21);
    }
    ((unsigned *)pixels)[0] = 0xFF112233u;
    ((unsigned *)pixels)[1] = 0xFF445566u;

    /* wl_shm.create_pool(new_id, fd, size), passing the descriptor. */
    begin(4, 0);
    put_u32(7);
    put_i32(4096);
    finish();
    if (send_message(display, (int)pool) <= 0) {
        return fail("wayland: create_pool failed\n", 27);
    }

    /* wl_shm_pool.create_buffer(new_id, offset, w, h, stride, format) */
    begin(7, 0);
    put_u32(8);
    put_i32(0);
    put_i32(8);
    put_i32(8);
    put_i32(32);
    put_u32(ARGB8888);
    finish();
    send_message(display, -1);

    /* wl_surface.attach(buffer, 0, 0); damage; commit. */
    begin(6, 1);
    put_u32(8);
    put_i32(0);
    put_i32(0);
    finish();
    send_message(display, -1);

    begin(6, 2);
    put_i32(0);
    put_i32(0);
    put_i32(8);
    put_i32(8);
    finish();
    send_message(display, -1);

    begin(6, 6);
    finish();
    send_message(display, -1);

    if (!client_roundtrip(display, 9)) {
        return fail("wayland: commit sync failed\n", 27);
    }

    sys3(57, display, 0, 0);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}

void _start(void) {
    long server = sys3(198, 1, 1, 0);
    if (server < 0 || bind_path(server) < 0) {
        fail("wayland: bind failed\n", 21);
    }
    if (sys3(201 /* listen */, server, 4, 0) < 0) {
        fail("wayland: listen failed\n", 23);
    }

    long child = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (child < 0) {
        fail("wayland: fork failed\n", 21);
    }
    if (child == 0) {
        run_client();
    }

    long client = sys3(202 /* accept */, server, 0, 0);
    if (client < 0) {
        fail("wayland: accept failed\n", 23);
    }
    int okay = serve(client);
    sys3(57, client, 0, 0);
    sys3(57, server, 0, 0);

    int status = 0;
    if (sys6(260 /* wait4 */, child, (long)&status, 0, 0, 0, 0) != child) {
        fail("wayland: wait failed\n", 21);
    }
    if (!okay) {
        fail("wayland: pixels not verified\n", 30);
    }
    if ((status & 0x7f) != 0 || ((status >> 8) & 0xff) != 0) {
        fail("wayland: client failed\n", 23);
    }

    sys3(64, 1, (long)"wayland ok\n", 11);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
