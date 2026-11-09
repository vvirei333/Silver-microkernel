/* D-Bus authentication and a Hello method call over the Linux ABI.
 *
 *   linuxrun /sfs/linux-dbus
 *
 * The parent is a minimal bus daemon: it accepts a named AF_UNIX connection,
 * checks SO_PEERCRED, performs the EXTERNAL handshake, parses the client's
 * org.freedesktop.DBus.Hello method call, and answers with a method_return
 * carrying the unique name ":1.0". The child is the client.
 *
 * Expected: "dbus ok" and exit 0.
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

struct sockaddr_un {
    unsigned short family;
    char path[108];
};

static const char NAME[] = "/tmp/dbus-test";
static const char GUID[] = "0123456789abcdef0123456789abcdef";

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
    return sys3(200, socket, (long)&address, sizeof(unsigned short) + length + 1);
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
    return sys3(203, socket, (long)&address, sizeof(unsigned short) + length + 1);
}

static int text_equal(const char *left, const char *right) {
    while (*left != 0 && *left == *right) {
        left++;
        right++;
    }
    return *left == *right;
}

/* ------------------------------------------------------------ message build */

static unsigned char message[1024];
static unsigned message_length;
static unsigned char body[256];
static unsigned body_length;

static void put_u32(unsigned value) {
    message[message_length++] = value & 0xff;
    message[message_length++] = (value >> 8) & 0xff;
    message[message_length++] = (value >> 16) & 0xff;
    message[message_length++] = (value >> 24) & 0xff;
}
static void align_to(unsigned alignment) {
    while (message_length % alignment != 0) {
        message[message_length++] = 0;
    }
}
static void put_text(const char *text, unsigned with_nul) {
    unsigned length = 0;
    while (text[length] != 0) {
        length++;
    }
    length += with_nul;
    unsigned encoded = length;
    message[message_length++] = encoded & 0xff;
    message[message_length++] = (encoded >> 8) & 0xff;
    message[message_length++] = (encoded >> 16) & 0xff;
    message[message_length++] = (encoded >> 24) & 0xff;
    for (unsigned index = 0; index < length; index++) {
        message[message_length++] =
            index < (length - with_nul) ? (unsigned char)text[index] : 0;
    }
}

static void message_begin(unsigned type, unsigned serial) {
    message_length = 0;
    message[message_length++] = 'l';
    message[message_length++] = (unsigned char)type;
    message[message_length++] = 0;
    message[message_length++] = 1;
    put_u32(0); /* body length, patched by finish */
    put_u32(serial);
    put_u32(0); /* header fields length, patched by finish */
}

static void field_text(unsigned code, const char *text, int path) {
    align_to(8);
    message[message_length++] = (unsigned char)code;
    message[message_length++] = 1;
    message[message_length++] = path ? 'o' : 's';
    message[message_length++] = 0;
    align_to(4);
    put_text(text, 1);
}

static void field_u32(unsigned code, unsigned value) {
    align_to(8);
    message[message_length++] = (unsigned char)code;
    message[message_length++] = 1;
    message[message_length++] = 'u';
    message[message_length++] = 0;
    align_to(4);
    put_u32(value);
}

static void field_signature(unsigned code, char signature) {
    align_to(8);
    message[message_length++] = (unsigned char)code;
    /* The variant's own type is a signature. */
    message[message_length++] = 1;
    message[message_length++] = 'g';
    message[message_length++] = 0;
    /* The value is the signature "s". */
    message[message_length++] = 1;
    message[message_length++] = (unsigned char)signature;
    message[message_length++] = 0;
}

static void body_begin(void) { body_length = 0; }
static void body_text(const char *text) {
    unsigned length = 0;
    while (text[length] != 0) {
        length++;
    }
    length++;
    body[body_length++] = length & 0xff;
    body[body_length++] = (length >> 8) & 0xff;
    body[body_length++] = (length >> 16) & 0xff;
    body[body_length++] = (length >> 24) & 0xff;
    for (unsigned index = 0; index < length; index++) {
        body[body_length++] =
            index + 1 < length ? (unsigned char)text[index] : 0;
    }
}

static unsigned message_finish(void) {
    unsigned fields = message_length - 16;
    message[12] = fields & 0xff;
    message[13] = (fields >> 8) & 0xff;
    message[14] = (fields >> 16) & 0xff;
    message[15] = (fields >> 24) & 0xff;
    align_to(8);
    message[4] = body_length & 0xff;
    message[5] = (body_length >> 8) & 0xff;
    message[6] = (body_length >> 16) & 0xff;
    message[7] = (body_length >> 24) & 0xff;
    for (unsigned index = 0; index < body_length; index++) {
        message[message_length++] = body[index];
    }
    return message_length;
}

/* ------------------------------------------------------------- message read */

static unsigned char incoming[1024];
static unsigned incoming_length;
static unsigned offset;

static unsigned take_u32(void) {
    unsigned value = incoming[offset] | (incoming[offset + 1] << 8) |
                     (incoming[offset + 2] << 16) |
                     (incoming[offset + 3] << 24);
    offset += 4;
    return value;
}
static void align_read(unsigned alignment) {
    while (offset % alignment != 0) {
        offset++;
    }
}
static void take_string(char *output, unsigned capacity) {
    unsigned length = take_u32();
    unsigned copied = 0;
    while (copied + 1 < capacity && copied < length) {
        output[copied] = (char)incoming[offset + copied];
        copied++;
    }
    output[copied] = 0;
    offset += length;
}
static void take_signature(char *output, unsigned capacity) {
    unsigned length = incoming[offset++];
    unsigned copied = 0;
    while (copied + 1 < capacity && copied < length) {
        output[copied] = (char)incoming[offset + copied];
        copied++;
    }
    output[copied] = 0;
    offset += length;
    offset++; /* NUL */
}

struct fields {
    char path[128];
    char interface[128];
    char member[128];
    char destination[128];
    char signature[16];
    unsigned reply_serial;
    int has_reply_serial;
};

static int parse_message(struct fields *result) {
    for (unsigned index = 0; index < sizeof(*result); index++) {
        ((unsigned char *)result)[index] = 0;
    }
    if (incoming_length < 16 || incoming[0] != 'l' || incoming[3] != 1) {
        return 0;
    }
    unsigned fields_length = incoming[12] | (incoming[13] << 8) |
                             (incoming[14] << 16) | (incoming[15] << 24);
    offset = 16;
    unsigned end = 16 + fields_length;
    if (end > incoming_length) {
        return 0;
    }
    while (offset < end) {
        align_read(8);
        unsigned code = incoming[offset++];
        char signature[8];
        take_signature(signature, sizeof(signature));
        if (signature[0] == 's' || signature[0] == 'o') {
            align_read(4);
            char text[128];
            take_string(text, sizeof(text));
            if (code == 1) {
                unsigned index = 0;
                while (text[index] != 0 && index + 1 < sizeof(result->path)) {
                    result->path[index] = text[index];
                    index++;
                }
                result->path[index] = 0;
            } else if (code == 2) {
                unsigned index = 0;
                while (text[index] != 0 &&
                       index + 1 < sizeof(result->interface)) {
                    result->interface[index] = text[index];
                    index++;
                }
                result->interface[index] = 0;
            } else if (code == 3) {
                unsigned index = 0;
                while (text[index] != 0 &&
                       index + 1 < sizeof(result->member)) {
                    result->member[index] = text[index];
                    index++;
                }
                result->member[index] = 0;
            } else if (code == 6) {
                unsigned index = 0;
                while (text[index] != 0 &&
                       index + 1 < sizeof(result->destination)) {
                    result->destination[index] = text[index];
                    index++;
                }
                result->destination[index] = 0;
            }
        } else if (signature[0] == 'u') {
            align_read(4);
            unsigned value = take_u32();
            if (code == 5) {
                result->reply_serial = value;
                result->has_reply_serial = 1;
            }
        } else if (signature[0] == 'g') {
            char nested[16];
            take_signature(nested, sizeof(nested));
            if (code == 8) {
                unsigned index = 0;
                while (nested[index] != 0 &&
                       index + 1 < sizeof(result->signature)) {
                    result->signature[index] = nested[index];
                    index++;
                }
                result->signature[index] = 0;
            }
        } else {
            return 0;
        }
    }
    return 1;
}

/* ---------------------------------------------------------------- transport */

static long send_buffer(long fd, const unsigned char *bytes, unsigned length) {
    unsigned sent = 0;
    while (sent < length) {
        long count = sys3(64 /* write */, fd, (long)(bytes + sent), length - sent);
        if (count <= 0) {
            return -1;
        }
        sent += (unsigned)count;
    }
    return sent;
}

static long receive_buffer(long fd) {
    long count = sys3(63 /* read */, fd, (long)incoming, sizeof(incoming));
    if (count <= 0) {
        return count;
    }
    incoming_length = (unsigned)count;
    return count;
}

/* ------------------------------------------------------------------- server */

static long server_socket;

static int server_authenticate(long client) {
    unsigned char credential[12];
    unsigned credential_length = sizeof(credential);
    if (sys6(209 /* getsockopt */, client, 1 /* SOL_SOCKET */, 17 /* SO_PEERCRED */,
             (long)credential, (long)&credential_length, 0) < 0) {
        return 0;
    }
    unsigned uid = credential[4] | (credential[5] << 8) | (credential[6] << 16) |
                   (credential[7] << 24);
    if (credential_length != 12 || uid != 0) {
        return 0;
    }
    if (receive_buffer(client) <= 0) {
        return 0;
    }
    /* "\0AUTH EXTERNAL 30\r\n" */
    if (incoming_length < 19 || incoming[0] != 0) {
        return 0;
    }
    const char expected[] = "AUTH EXTERNAL 30\r\n";
    for (unsigned index = 0; expected[index] != 0; index++) {
        if (incoming[index + 1] != (unsigned char)expected[index]) {
            return 0;
        }
    }
    unsigned char ok[64];
    unsigned length = 0;
    ok[length++] = 'O';
    ok[length++] = 'K';
    ok[length++] = ' ';
    for (unsigned index = 0; GUID[index] != 0; index++) {
        ok[length++] = (unsigned char)GUID[index];
    }
    ok[length++] = '\r';
    ok[length++] = '\n';
    if (send_buffer(client, ok, length) < 0) {
        return 0;
    }
    if (receive_buffer(client) < 6 || incoming[0] != 'B') {
        return 0;
    }
    return 1;
}

static int server_run(long client) {
    if (!server_authenticate(client)) {
        return 0;
    }
    if (receive_buffer(client) <= 0) {
        return 0;
    }
    if (incoming[1] != 1 /* method_call */) {
        return 0;
    }
    struct fields call;
    if (!parse_message(&call)) {
        return 0;
    }
    if (!text_equal(call.member, "Hello") ||
        !text_equal(call.path, "/org/freedesktop/DBus") ||
        !text_equal(call.interface, "org.freedesktop.DBus") ||
        !text_equal(call.destination, "org.freedesktop.DBus")) {
        return 0;
    }

    /* Method return with the unique name in the body. */
    message_begin(2 /* method_return */, 2);
    field_u32(5 /* REPLY_SERIAL */, 1);
    field_text(7 /* SENDER */, "org.freedesktop.DBus", 0);
    field_signature(8 /* SIGNATURE */, 's');
    body_begin();
    body_text(":1.0");
    unsigned length = message_finish();
    return send_buffer(client, message, length) > 0;
}

/* ------------------------------------------------------------------- client */

static int client_run(void) {
    long client = sys3(198, 1, 1, 0);
    if (client < 0 || connect_path(client) < 0) {
        return fail("dbus: connect failed\n", 20);
    }
    if (send_buffer(client, (const unsigned char *)"\0AUTH EXTERNAL 30\r\n", 19) < 0) {
        return fail("dbus: auth write failed\n", 24);
    }
    if (receive_buffer(client) <= 0 || incoming[0] != 'O') {
        return fail("dbus: auth rejected\n", 20);
    }
    if (send_buffer(client, (const unsigned char *)"BEGIN\r\n", 7) < 0) {
        return fail("dbus: begin failed\n", 19);
    }

    message_begin(1 /* method_call */, 1);
    field_text(1 /* PATH */, "/org/freedesktop/DBus", 1);
    field_text(2 /* INTERFACE */, "org.freedesktop.DBus", 0);
    field_text(3 /* MEMBER */, "Hello", 0);
    field_text(6 /* DESTINATION */, "org.freedesktop.DBus", 0);
    body_begin();
    unsigned length = message_finish();
    if (send_buffer(client, message, length) < 0) {
        return fail("dbus: hello failed\n", 19);
    }

    if (receive_buffer(client) <= 0) {
        return fail("dbus: no reply\n", 15);
    }
    if (incoming[1] != 2 /* method_return */) {
        return fail("dbus: wrong reply type\n", 22);
    }
    struct fields reply;
    if (!parse_message(&reply) || !reply.has_reply_serial ||
        reply.reply_serial != 1) {
        return fail("dbus: reply serial\n", 20);
    }
    if (!text_equal(reply.signature, "s")) {
        return fail("dbus: reply signature\n", 23);
    }
    unsigned body_start = 16 + (incoming[12] | (incoming[13] << 8) |
                                (incoming[14] << 16) | (incoming[15] << 24));
    while (body_start % 8 != 0) {
        body_start++;
    }
    offset = body_start;
    char unique[32];
    take_string(unique, sizeof(unique));
    if (unique[0] != ':' || unique[1] == 0) {
        return fail("dbus: bad unique name\n", 23);
    }

    sys3(57, client, 0, 0);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}

void _start(void) {
    server_socket = sys3(198, 1, 1, 0);
    if (server_socket < 0 || bind_path(server_socket) < 0) {
        fail("dbus: bind failed\n", 18);
    }
    if (sys3(201 /* listen */, server_socket, 4, 0) < 0) {
        fail("dbus: listen failed\n", 20);
    }

    long child = sys6(220 /* clone */, 17, 0, 0, 0, 0, 0);
    if (child < 0) {
        fail("dbus: fork failed\n", 18);
    }
    if (child == 0) {
        client_run();
    }

    long client = sys3(202 /* accept */, server_socket, 0, 0);
    if (client < 0) {
        fail("dbus: accept failed\n", 20);
    }
    int okay = server_run(client);
    sys3(57, client, 0, 0);
    sys3(57, server_socket, 0, 0);

    int status = 0;
    if (sys6(260 /* wait4 */, child, (long)&status, 0, 0, 0, 0) != child) {
        fail("dbus: wait failed\n", 18);
    }
    if (!okay) {
        fail("dbus: server rejected\n", 22);
    }
    if ((status & 0x7f) != 0 || ((status >> 8) & 0xff) != 0) {
        fail("dbus: client failed\n", 20);
    }

    sys3(64, 1, (long)"dbus ok\n", 8);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
