/* Wire/syscall helpers for the minimal Wayland client (freestanding). */
#pragma once

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

#define MAX_INCOMING_FDS 8
static int incoming_fds[MAX_INCOMING_FDS];
static unsigned incoming_fd_count;

static unsigned char message[4096];
static unsigned message_length;

#define INCOMING_SIZE 65536
static unsigned char incoming[INCOMING_SIZE];
static unsigned long incoming_length;
static unsigned long incoming_offset;
static unsigned incoming_pending;

static unsigned compositor_version;
static unsigned message_start;

static void put_u32(unsigned value) {
    message[message_length++] = value & 0xff;
    message[message_length++] = (value >> 8) & 0xff;
    message[message_length++] = (value >> 16) & 0xff;
    message[message_length++] = (value >> 24) & 0xff;
}
static void put_i32(int value) { put_u32((unsigned)value); }
static void put_u16(unsigned value) {
    message[message_length++] = value & 0xff;
    message[message_length++] = (value >> 8) & 0xff;
}
static void put_new_id(unsigned value) { put_u32(value); }
static void put_string(const char *text) {
    unsigned length = 0;
    while (text[length] != 0) length++;
    length++;
    put_u32(length);
    for (unsigned index = 0; index < length; index++) {
        message[message_length++] = (unsigned char)text[index];
    }
    while (message_length % 4 != 0) message[message_length++] = 0;
}
static void begin(unsigned object, unsigned opcode) {
    message_length = 0;
    put_u32(object);
    put_u16(opcode);
    put_u16(8);
}
static void finish(void) {
    while (message_length % 4 != 0) message[message_length++] = 0;
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
        control.header.level = 1;
        control.header.kind = 1;
        *(int *)((unsigned char *)&control.header + sizeof(struct cmsghdr)) = pass_fd;
        header.control = control.raw;
        header.control_length = sizeof(struct cmsghdr) + sizeof(int);
    }
    return sys3(211, fd, (long)&header, 0);
}

static long receive_message(long fd) {
    struct { long seconds; long nanoseconds; } timeout = { 0, 20000000 };
    struct { int fd; short events; short revents; } ready;
    ready.fd = (int)fd;
    ready.events = 1;
    ready.revents = 0;
    long polled = sys6(73, (long)&ready, 1, (long)&timeout, 0, 0, 0);
    if (polled == 0) return -2;
    if (polled < 0) return -1;
    struct iovec vector = {incoming + incoming_pending, INCOMING_SIZE - incoming_pending};
    struct msghdr header;
    union control control;
    for (unsigned long index = 0; index < sizeof(header); index++) {
        ((unsigned char *)&header)[index] = 0;
    }
    header.iov = &vector;
    header.iov_length = 1;
    header.control = control.raw;
    header.control_length = sizeof(control.raw);
    long count = sys3(212, fd, (long)&header, 0);
    if (count <= 0) return count;
    incoming_length = incoming_pending + (unsigned long)count;
    incoming_pending = 0;
    incoming_offset = 8;
    incoming_fd_count = 0;
    unsigned long control_offset = 0;
    while (control_offset + sizeof(struct cmsghdr) <= header.control_length) {
        struct cmsghdr *got = (struct cmsghdr *)(control.raw + control_offset);
        if (got->length < sizeof(struct cmsghdr) ||
            control_offset + got->length > header.control_length) break;
        if (got->level == 1 && got->kind == 1) {
            unsigned long bytes = got->length - sizeof(struct cmsghdr);
            unsigned count = (unsigned)(bytes / sizeof(int));
            int *fds = (int *)(control.raw + control_offset + sizeof(struct cmsghdr));
            for (unsigned index = 0; index < count && incoming_fd_count < MAX_INCOMING_FDS; index++) {
                incoming_fds[incoming_fd_count++] = fds[index];
            }
        }
        control_offset += (got->length + 7ul) & ~7ul;
    }
    return count;
}

/* Returns 1 when one complete message is framed; advances the cursor. */
static int parse_next_message(void) {
    if (incoming_offset + 8 > incoming_length) return 0;
    unsigned size = incoming[incoming_offset + 6] | (incoming[incoming_offset + 7] << 8);
    if (size < 8 || incoming_offset + size > incoming_length) return 0;
    message_start = incoming_offset;
    incoming_offset += size;
    return 1;
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
    return incoming[message_start] | (incoming[message_start + 1] << 8) |
           (incoming[message_start + 2] << 16) | (incoming[message_start + 3] << 24);
}
static unsigned opcode_of(void) {
    return incoming[message_start + 4] | (incoming[message_start + 5] << 8);
}
static int text_equal(const char *left, const char *right) {
    while (*left != 0 && *left == *right) { left++; right++; }
    return *left == *right;
}

static long connect_path(long socket, const char *name) {
    struct { unsigned short family; char path[108]; } address;
    for (unsigned index = 0; index < sizeof(address.path); index++) address.path[index] = 0;
    address.family = 1;
    unsigned length = 0;
    while (name[length]) { address.path[length] = name[length]; length++; }
    return sys3(203, socket, (long)&address, sizeof(unsigned short) + length + 1);
}
