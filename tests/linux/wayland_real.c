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
static unsigned message_start;
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
    return incoming[message_start] | (incoming[message_start + 1] << 8) |
           (incoming[message_start + 2] << 16) |
           (incoming[message_start + 3] << 24);
}
static unsigned opcode_of(void) {
    return incoming[message_start + 4] | (incoming[message_start + 5] << 8);
}

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

static unsigned registry_id;
static unsigned compositor_id;
static unsigned shm_id;
static unsigned shell_id;
static unsigned xdg_surface_id;
static unsigned toplevel_id;
static unsigned output_id;
static unsigned output_version;
static unsigned data_manager_id;
static unsigned data_device_id;
static unsigned decoration_manager_id;
static unsigned decoration_id;
static unsigned viewporter_id;
static unsigned viewport_id;
static unsigned subcompositor_id;
static unsigned subsurface_id;
static unsigned xdg_output_manager_id;
static unsigned xdg_output_id;
static unsigned activation_id;
static unsigned activation_token_id;
static unsigned fractional_manager_id;
static unsigned fractional_id;
static unsigned subsurface_surface_id;
static int subsurface_ok;
static unsigned data_source_id;
static unsigned data_offer_id;
static char data_mimes[4][64];
static unsigned data_mime_count;
static unsigned seat_id;
static unsigned seat_version;
static unsigned keyboard_id;
static unsigned pointer_id;
static unsigned configure_serial = 1;
static unsigned surface_id;
static unsigned popup_surface_id;
static unsigned popup_xdg_surface_id;
static unsigned popup_id;
static unsigned positioner_id;
static int popup_grab;
static int popup_committed;
static unsigned pool_id;
static unsigned buffer_id;
static unsigned frame_id;
static int pool_fd = -1;
static int verified;
static unsigned commits;
static int focused;
static int surface_entered;
static int selection_changed;
static int wants_offer;
static int current_client = -1;

typedef struct {
    unsigned registry_id;
    unsigned compositor_id;
    unsigned shm_id;
    unsigned shell_id;
    unsigned xdg_surface_id;
    unsigned toplevel_id;
    unsigned output_id;
    unsigned output_version;
    unsigned data_manager_id;
    unsigned data_device_id;
    unsigned decoration_manager_id;
    unsigned decoration_id;
    unsigned viewporter_id;
    unsigned viewport_id;
    unsigned subcompositor_id;
    unsigned subsurface_id;
    unsigned xdg_output_manager_id;
    unsigned xdg_output_id;
    unsigned activation_id;
    unsigned activation_token_id;
    unsigned fractional_manager_id;
    unsigned fractional_id;
    unsigned subsurface_surface_id;
    int subsurface_ok;
    unsigned data_source_id;
    unsigned data_offer_id;
    unsigned seat_id;
    unsigned seat_version;
    unsigned keyboard_id;
    unsigned pointer_id;
    unsigned configure_serial;
    unsigned surface_id;
    unsigned popup_surface_id;
    unsigned popup_xdg_surface_id;
    unsigned popup_id;
    unsigned positioner_id;
    int popup_grab;
    int popup_committed;
    unsigned pool_id;
    unsigned buffer_id;
    unsigned frame_id;
    int pool_fd;
    int verified;
    unsigned commits;
    int focused;
    int surface_entered;
    unsigned data_mime_count;
    char data_mimes[4][64];
    long fd;
    unsigned next_server_id;
    int connected;
    int selection_changed;
    int wants_offer;
} ClientState;

#define CLIENT_SCALARS(X) \
    X(registry_id) X(compositor_id) X(shm_id) X(shell_id) \
    X(xdg_surface_id) X(toplevel_id) X(output_id) X(output_version) \
    X(data_manager_id) X(data_device_id) X(data_source_id) X(data_offer_id) \
    X(decoration_manager_id) X(decoration_id) X(viewporter_id) X(viewport_id) \
    X(subcompositor_id) X(subsurface_id) X(subsurface_surface_id) X(subsurface_ok) \
    X(xdg_output_manager_id) X(xdg_output_id) \
    X(activation_id) X(activation_token_id) \
    X(fractional_manager_id) X(fractional_id) \
    X(seat_id) X(seat_version) X(keyboard_id) X(pointer_id) \
    X(configure_serial) X(surface_id) X(popup_surface_id) \
    X(popup_xdg_surface_id) X(popup_id) X(positioner_id) \
    X(popup_grab) X(popup_committed) X(pool_id) X(buffer_id) X(frame_id) \
    X(pool_fd) X(verified) X(commits) X(focused) X(surface_entered) \
    X(data_mime_count) X(selection_changed) X(wants_offer)

#define MAX_CLIENTS 2
static ClientState clients[MAX_CLIENTS];
static int clipboard_owner = -1;
static unsigned clipboard_source_id;
static char clipboard_mimes[4][64];
static unsigned clipboard_mime_count;

static void copy_mimes(char destination[4][64], char source[4][64]) {
    for (unsigned outer = 0; outer < 4; outer++) {
        for (unsigned inner = 0; inner < 64; inner++) {
            destination[outer][inner] = source[outer][inner];
        }
    }
}

static void load_client(ClientState *client) {
#define LOAD(field) field = client->field;
    CLIENT_SCALARS(LOAD)
#undef LOAD
    copy_mimes(data_mimes, client->data_mimes);
}

static void save_client(ClientState *client) {
#define SAVE(field) client->field = field;
    CLIENT_SCALARS(SAVE)
#undef SAVE
    copy_mimes(client->data_mimes, data_mimes);
}

static void send_global_version(long client, unsigned registry, unsigned name,
                                const char *interface, unsigned version) {
    begin(registry, 0 /* wl_registry.global */);
    put_u32(name);
    put_string(interface);
    put_u32(version);
    finish();
    send_message(client, -1);
}

static void send_global(long client, unsigned registry, unsigned name,
                        const char *interface) {
    send_global_version(client, registry, name, interface, 1);
}

static void send_done(long client, unsigned callback, unsigned data) {
    begin(callback, 0 /* wl_callback.done */);
    put_u32(data);
    finish();
    send_message(client, -1);
}

/// Sends the current shared selection to one client, allocating the next
/// server-range object id for its data offer.
static void send_clipboard_offer(ClientState *client) {
    if (!client->connected || client->data_device_id == 0 ||
        clipboard_owner < 0 || clipboard_mime_count == 0) {
        return;
    }
    unsigned offer = client->next_server_id++;
    client->data_offer_id = offer;
    begin(client->data_device_id, 0 /* wl_data_device.data_offer */);
    put_u32(offer);
    finish();
    send_message(client->fd, -1);
    for (unsigned mime = 0; mime < clipboard_mime_count; mime++) {
        begin(offer, 0 /* wl_data_offer.offer */);
        put_string(clipboard_mimes[mime]);
        finish();
        send_message(client->fd, -1);
    }
    begin(client->data_device_id, 5 /* wl_data_device.selection */);
    put_u32(offer);
    finish();
    send_message(client->fd, -1);
}

static void broadcast_clipboard(void) {
    for (int index = 0; index < MAX_CLIENTS; index++) {
        send_clipboard_offer(&clients[index]);
    }
}

/// Sends a toplevel configure with zero or one state, then the matching
/// xdg_surface.configure serial.
static void send_toplevel_state(long client, unsigned state) {
    begin(toplevel_id, 0 /* xdg_toplevel.configure */);
    put_i32(1024);
    put_i32(768);
    put_u32(state != 0 ? 1 : 0);
    if (state != 0) {
        put_u32(state);
    }
    finish();
    send_message(client, -1);
    begin(xdg_surface_id, 0 /* xdg_surface.configure */);
    put_u32(configure_serial);
    configure_serial += 1;
    finish();
    send_message(client, -1);
}

static void send_decoration_mode(long client, unsigned mode) {
    if (decoration_id == 0) {
        return;
    }
    begin(decoration_id, 0 /* zxdg_toplevel_decoration_v1.configure */);
    put_u32(mode);
    finish();
    send_message(client, -1);
}

static int serve(long client) {
    long count = receive_message(client);
    if (count == 0) {
        return 1; /* peer closed */
    }
    if (count < 0) {
        return -1; /* protocol/transport error */
    }
    {
        /* One flush may contain several protocol messages. */
        message_start = 0;
        while (message_start + 8 <= incoming_length) {
            unsigned size = incoming[message_start + 6] |
                            (incoming[message_start + 7] << 8);
            if (size < 8 || message_start + size > incoming_length) {
                break;
            }
            unsigned object = object_of();
            unsigned opcode = opcode_of();
            incoming_offset = message_start + 8;

            if (object == 1 && opcode == 0) { /* wl_display.sync */
                unsigned callback = take_u32();
                send_done(client, callback, 0);
            } else if (object == 1 && opcode == 1) { /* wl_display.get_registry */
                registry_id = take_u32();
                send_global_version(client, registry_id, 1, "wl_compositor", 4);
                send_global(client, registry_id, 2, "wl_shm");
                send_global(client, registry_id, 3, "xdg_wm_base");
                send_global_version(client, registry_id, 4, "wl_output", 4);
                send_global_version(client, registry_id, 5, "wl_seat", 5);
                send_global(client, registry_id, 6, "wl_data_device_manager");
                send_global_version(client, registry_id, 7,
                                    "zxdg_decoration_manager_v1", 2);
                send_global(client, registry_id, 8, "wp_viewporter");
                send_global(client, registry_id, 9, "wl_subcompositor");
                send_global_version(client, registry_id, 10,
                                    "zxdg_output_manager_v1", 3);
                send_global(client, registry_id, 11, "xdg_activation_v1");
                send_global(client, registry_id, 12,
                            "wp_fractional_scale_manager_v1");
            } else if (object == registry_id && opcode == 0) { /* bind */
                (void)take_u32();
                char interface[64];
                take_string(interface, sizeof(interface));
                unsigned version = take_u32();
                unsigned new_id = take_u32();
                if (text_equal(interface, "wl_compositor")) {
                    compositor_id = new_id;
                } else if (text_equal(interface, "wl_shm")) {
                    shm_id = new_id;
                    begin(shm_id, 0 /* wl_shm.format */);
                    put_u32(0); /* ARGB8888 */
                    finish();
                    send_message(client, -1);
                    begin(shm_id, 0 /* wl_shm.format */);
                    put_u32(1); /* XRGB8888 */
                    finish();
                    send_message(client, -1);
                } else if (text_equal(interface, "xdg_wm_base")) {
                    shell_id = new_id;
                } else if (text_equal(interface, "wl_output")) {
                    output_id = new_id;
                    output_version = version;
                    begin(output_id, 0 /* geometry */);
                    put_i32(0); put_i32(0);
                    put_i32(300); put_i32(200);
                    put_i32(0); /* subpixel */
                    put_string("Werewolf"); put_string("Panel");
                    put_i32(0); /* transform */
                    finish(); send_message(client, -1);
                    begin(output_id, 1 /* mode */);
                    put_u32(3); put_i32(1024); put_i32(768); put_i32(60000);
                    finish(); send_message(client, -1);
                    if (output_version >= 2) {
                        begin(output_id, 3 /* scale */);
                        put_i32(2);
                        finish(); send_message(client, -1);
                    }
                    if (output_version >= 4) {
                        begin(output_id, 4 /* name */);
                        put_string("Werewolf panel");
                        finish(); send_message(client, -1);
                        begin(output_id, 5 /* description */);
                        put_string("WerewolfOS 1024x768 panel");
                        finish(); send_message(client, -1);
                    }
                    if (output_version >= 2) {
                        begin(output_id, 2 /* done */);
                        finish(); send_message(client, -1);
                    }
                } else if (text_equal(interface, "wl_seat")) {
                    seat_id = new_id;
                    seat_version = version;
                    if (seat_version >= 2) {
                        begin(seat_id, 1 /* name */);
                        put_string("Werewolf seat");
                        finish(); send_message(client, -1);
                    }
                    begin(seat_id, 0 /* capabilities */);
                    put_u32(3); /* pointer | keyboard */
                    finish(); send_message(client, -1);
                } else if (text_equal(interface, "wl_data_device_manager")) {
                    data_manager_id = new_id;
                } else if (text_equal(interface, "zxdg_decoration_manager_v1")) {
                    decoration_manager_id = new_id;
                } else if (text_equal(interface, "wp_viewporter")) {
                    viewporter_id = new_id;
                } else if (text_equal(interface, "wl_subcompositor")) {
                    subcompositor_id = new_id;
                } else if (text_equal(interface, "zxdg_output_manager_v1")) {
                    xdg_output_manager_id = new_id;
                } else if (text_equal(interface, "xdg_activation_v1")) {
                    activation_id = new_id;
                } else if (text_equal(interface,
                                      "wp_fractional_scale_manager_v1")) {
                    fractional_manager_id = new_id;
                }
            } else if (object == fractional_manager_id && opcode == 1) {
                fractional_id = take_u32(); /* get_fractional_scale */
                (void)take_u32(); /* surface */
                begin(fractional_id, 0 /* preferred_scale */);
                put_u32(240); /* 2.0 */
                finish(); send_message(client, -1);
            } else if (object == fractional_id && opcode == 0) {
                fractional_id = 0;
            } else if (object == activation_id && opcode == 1) {
                activation_token_id = take_u32(); /* get_activation_token */
            } else if (object == activation_token_id && opcode == 3) {
                begin(activation_token_id, 0 /* xdg_activation_token_v1.done */);
                put_string("werewolf-token");
                finish(); send_message(client, -1);
            } else if (object == activation_token_id && opcode == 4) {
                activation_token_id = 0;
            } else if (object == activation_id && opcode == 2) {
                char token[64];
                take_string(token, sizeof(token));
                (void)take_u32(); /* surface */
            } else if (object == xdg_output_manager_id && opcode == 1) {
                /* zxdg_output_manager_v1.get_xdg_output(new_id, output) */
                xdg_output_id = take_u32();
                (void)take_u32();
                begin(xdg_output_id, 0 /* logical_position */);
                put_i32(0); put_i32(0);
                finish(); send_message(client, -1);
                begin(xdg_output_id, 1 /* logical_size */);
                put_i32(1024); put_i32(768);
                finish(); send_message(client, -1);
                begin(xdg_output_id, 3 /* name */);
                put_string("Werewolf panel");
                finish(); send_message(client, -1);
                begin(xdg_output_id, 4 /* description */);
                put_string("WerewolfOS 1024x768 panel");
                finish(); send_message(client, -1);
                if (output_id != 0) {
                    begin(output_id, 2 /* wl_output.done */);
                    finish(); send_message(client, -1);
                }
            } else if (object == xdg_output_id && opcode == 0) {
                xdg_output_id = 0;
            } else if (object == subcompositor_id && opcode == 1) {
                /* wl_subcompositor.get_subsurface(new_id, surface, parent) */
                subsurface_id = take_u32();
                subsurface_surface_id = take_u32();
                (void)take_u32();
            } else if (object == subsurface_id && opcode == 0) {
                subsurface_id = 0;
            } else if (object == decoration_manager_id && opcode == 1) {
                /* get_toplevel_decoration(new_id, toplevel) */
                decoration_id = take_u32();
                (void)take_u32();
                send_decoration_mode(client, 1 /* client-side */);
            } else if (object == decoration_id && opcode == 1) {
                unsigned mode = take_u32();
                if (mode == 1 || mode == 2) {
                    send_decoration_mode(client, mode);
                }
            } else if (object == decoration_id && opcode == 2) {
                send_decoration_mode(client, 1 /* client-side */);
            } else if (object == decoration_id && opcode == 0) {
                decoration_id = 0;
            } else if (object == viewporter_id && opcode == 1) {
                viewport_id = take_u32(); /* get_viewport */
                (void)take_u32(); /* surface */
            } else if (object == viewport_id && opcode == 1) {
                (void)take_i32(); (void)take_i32();
                (void)take_i32(); (void)take_i32(); /* set_source */
            } else if (object == viewport_id && opcode == 2) {
                (void)take_i32(); (void)take_i32(); /* set_destination */
            } else if (object == viewport_id && opcode == 0) {
                viewport_id = 0;
            } else if (object == data_manager_id && opcode == 0) {
                data_source_id = take_u32(); /* create_data_source */
            } else if (object == data_manager_id && opcode == 1) {
                data_device_id = take_u32(); /* get_data_device */
                (void)take_u32(); /* seat */
                wants_offer = 1;
            } else if (object == data_source_id && opcode == 0) {
                /* wl_data_source.offer(mime_type) */
                if (data_mime_count < 4) {
                    take_string(data_mimes[data_mime_count],
                                sizeof(data_mimes[data_mime_count]));
                    data_mime_count += 1;
                }
            } else if (object == data_source_id && opcode == 1) {
                data_source_id = 0;
                data_mime_count = 0;
                if (clipboard_owner == current_client) {
                    clipboard_owner = -1;
                    clipboard_mime_count = 0;
                }
            } else if (object == data_device_id && opcode == 1) {
                /* wl_data_device.set_selection(source, serial) */
                unsigned source = take_u32();
                (void)take_u32();
                if (source != 0 && source == data_source_id && data_mime_count != 0) {
                    clipboard_owner = current_client;
                    clipboard_source_id = source;
                    clipboard_mime_count = data_mime_count;
                    copy_mimes(clipboard_mimes, data_mimes);
                    selection_changed = 1;
                } else if (source == 0) {
                    clipboard_owner = -1;
                    clipboard_mime_count = 0;
                }
            } else if (object == data_device_id && opcode == 2) {
                data_device_id = 0;
            } else if (object == data_offer_id && opcode == 1) {
                /* wl_data_offer.receive(mime_type, fd) */
                char mime[64];
                take_string(mime, sizeof(mime));
                if (incoming_fd >= 0 && clipboard_owner >= 0 &&
                    clipboard_source_id != 0) {
                    begin(clipboard_source_id, 1 /* wl_data_source.send */);
                    put_string(mime);
                    finish();
                    send_message(clients[clipboard_owner].fd, incoming_fd);
                    sys3(57, incoming_fd, 0, 0);
                    incoming_fd = -1;
                }
            } else if (object == data_offer_id && opcode == 2) {
                data_offer_id = 0;
            } else if (object == compositor_id && opcode == 0) {
                unsigned new_surface = take_u32(); /* create_surface */
                if (surface_id == 0) {
                    surface_id = new_surface;
                } else if (popup_surface_id == 0) {
                    popup_surface_id = new_surface;
                } else if (subsurface_surface_id == 0) {
                    subsurface_surface_id = new_surface;
                }
            } else if (object == shell_id && opcode == 1) {
                positioner_id = take_u32(); /* create_positioner */
            } else if (object == shell_id && opcode == 2) {
                /* xdg_wm_base.get_xdg_surface(new_id, surface) */
                unsigned new_xdg_surface = take_u32();
                unsigned surface = take_u32();
                if (popup_surface_id != 0 && surface == popup_surface_id) {
                    popup_xdg_surface_id = new_xdg_surface;
                } else {
                    xdg_surface_id = new_xdg_surface;
                }
            } else if (object == xdg_surface_id && opcode == 1) {
                /* xdg_surface.get_toplevel(new_id): send configure. */
                toplevel_id = take_u32();
                begin(toplevel_id, 0 /* xdg_toplevel.configure */);
                put_i32(8); /* width */
                put_i32(8); /* height */
                put_u32(0); /* states array: empty */
                finish();
                send_message(client, -1);
                begin(xdg_surface_id, 0 /* xdg_surface.configure */);
                put_u32(configure_serial);
                configure_serial += 1;
                finish();
                send_message(client, -1);
            } else if (object == popup_xdg_surface_id && opcode == 2) {
                /* xdg_surface.get_popup(new_id, parent, positioner) */
                popup_id = take_u32();
                (void)take_u32();
                (void)take_u32();
                begin(popup_id, 0 /* xdg_popup.configure */);
                put_i32(40); put_i32(30); put_i32(200); put_i32(120);
                finish(); send_message(client, -1);
                begin(popup_xdg_surface_id, 0 /* xdg_surface.configure */);
                put_u32(configure_serial);
                configure_serial += 1;
                finish(); send_message(client, -1);
            } else if (object == xdg_surface_id && opcode == 4) {
                (void)take_u32(); /* ack_configure(serial) */
            } else if (object == popup_xdg_surface_id && opcode == 4) {
                (void)take_u32(); /* ack_configure(serial) */
            } else if (object == popup_id && opcode == 1) {
                (void)take_u32(); /* seat */
                (void)take_u32(); /* serial */
                popup_grab = 1;
                if (popup_committed) {
                    begin(popup_id, 1 /* xdg_popup.popup_done */);
                    finish(); send_message(client, -1);
                    popup_grab = 0;
                }
            } else if (object == popup_id && opcode == 0) {
                popup_id = 0;
            } else if (object == positioner_id && opcode == 0) {
                positioner_id = 0;
            } else if (object == seat_id && opcode == 1) {
                /* wl_seat.get_keyboard(new_id): keymap fd + enter. */
                keyboard_id = take_u32();
                const char text[] = "WEREWOLF_KEYMAP\n";
                unsigned length = sizeof(text) - 1;
                long keymap = sys3(279 /* memfd_create */, (long)"keymap", 0, 0);
                if (keymap >= 0 && sys3(46 /* ftruncate */, keymap, length, 0) >= 0 &&
                    sys3(64 /* write */, keymap, (long)text, length) == (long)length &&
                    sys3(62 /* lseek */, keymap, 0, 0) == 0) {
                    begin(keyboard_id, 0 /* keymap */);
                    put_u32(1); /* XKB_V1 */
                    put_u32(length);
                    finish();
                    send_message(client, (int)keymap);
                }
                if (keymap >= 0) {
                    sys3(57, keymap, 0, 0);
                }
                if (seat_version >= 4) {
                    begin(keyboard_id, 5 /* wl_keyboard.repeat_info */);
                    put_i32(25); /* rate */
                    put_i32(600); /* delay */
                    finish(); send_message(client, -1);
                }
            } else if (object == seat_id && opcode == 0) {
                /* wl_seat.get_pointer(new_id): enter + motion. */
                pointer_id = take_u32();
            } else if (object == shm_id && opcode == 0) {
                pool_id = take_u32(); /* create_pool */
                (void)take_i32();
                if (incoming_fd >= 0) {
                    if (pool_fd >= 0) {
                        sys3(57, pool_fd, 0, 0);
                    }
                    pool_fd = incoming_fd;
                }
            } else if (object == pool_id && opcode == 0) {
                buffer_id = take_u32(); /* create_buffer */
                (void)take_i32();
                (void)take_i32();
                (void)take_i32();
                (void)take_i32();
                (void)take_u32();
            } else if (object == surface_id && opcode == 0) {
                /* wl_surface.destroy: drop focus with leave events. */
                if (keyboard_id != 0) {
                    begin(keyboard_id, 2 /* leave */);
                    put_u32(configure_serial);
                    put_u32(surface_id);
                    finish();
                    send_message(client, -1);
                }
                if (pointer_id != 0) {
                    begin(pointer_id, 1 /* leave */);
                    put_u32(configure_serial);
                    put_u32(surface_id);
                    finish();
                    send_message(client, -1);
                }
                focused = 0;
                surface_id = 0;
            } else if (object == toplevel_id &&
                       (opcode == 9 || opcode == 10 || opcode == 11 || opcode == 12)) {
                /* set_maximized, unset_maximized, set_fullscreen, unset_fullscreen */
                if (opcode == 9) {
                    send_toplevel_state(client, 1 /* maximized */);
                } else if (opcode == 10) {
                    send_toplevel_state(client, 0);
                } else if (opcode == 11) {
                    (void)take_u32(); /* output */
                    send_toplevel_state(client, 2 /* fullscreen */);
                } else {
                    send_toplevel_state(client, 0);
                }
            } else if (object == subsurface_surface_id && opcode == 6) {
                subsurface_ok = 1;
            } else if (object == popup_surface_id && opcode == 6) {
                /* Popup commit: dismiss a grabbed popup once it is mapped. */
                popup_committed = 1;
                if (popup_grab && popup_id != 0) {
                    begin(popup_id, 1 /* xdg_popup.popup_done */);
                    finish(); send_message(client, -1);
                    popup_grab = 0;
                }
            } else if (object == popup_surface_id && opcode == 0) {
                popup_surface_id = 0;
                popup_xdg_surface_id = 0;
                popup_id = 0;
            } else if (object == surface_id && opcode == 3) {
                frame_id = take_u32(); /* wl_surface.frame */
            } else if (object == surface_id && opcode == 6) {
                /* wl_surface.commit: report the output, then verify pixels. */
                if (!surface_entered && output_id != 0) {
                    surface_entered = 1;
                    begin(surface_id, 0 /* wl_surface.enter */);
                    put_u32(output_id);
                    finish();
                    send_message(client, -1);
                }
                if (pool_fd >= 0 && buffer_id != 0) {
                    long mapped = sys6(222, 0, 4096, 1, 1, pool_fd, 0);
                    if (mapped > 0) {
                        unsigned *pixels = (unsigned *)mapped;
                        int first = pixels[0] == 0xFF112233u &&
                                    pixels[1] == 0xFF445566u;
                        int second = pixels[0] == 0xFF778899u &&
                                     pixels[1] == 0xFFAABBCCu;
                        if (first || second) {
                            verified = 1;
                        }
                        sys3(215, mapped, 4096, 0);
                    }
                    commits += 1;
                }
                if (buffer_id != 0 && !focused) {
                    focused = 1;
                    if (keyboard_id != 0) {
                        begin(keyboard_id, 1 /* enter */);
                        put_u32(configure_serial);
                        put_u32(surface_id);
                        put_u32(0); /* keys array, empty */
                        finish();
                        send_message(client, -1);
                    }
                    if (pointer_id != 0) {
                        begin(pointer_id, 0 /* enter */);
                        put_u32(configure_serial);
                        put_u32(surface_id);
                        put_i32(100 << 8);
                        put_i32(50 << 8);
                        finish();
                        send_message(client, -1);
                        begin(pointer_id, 2 /* motion */);
                        put_u32(1);
                        put_i32(100 << 8);
                        put_i32(50 << 8);
                        finish();
                        send_message(client, -1);
                    }
                }
                if (buffer_id != 0) {
                    begin(buffer_id, 0 /* wl_buffer.release */);
                    finish();
                    send_message(client, -1);
                }
                if (frame_id != 0) {
                    send_done(client, frame_id, 0);
                }
            }
            message_start += size;
        }
    }
    return 0;
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

/// Each accepted connection is a separate client namespace; reset the
/// per-surface state before serving the next one.
static void reset_client(void) {
    if (pool_fd >= 0) {
        sys3(57, pool_fd, 0, 0);
        pool_fd = -1;
    }
    registry_id = 0;
    compositor_id = 0;
    shm_id = 0;
    shell_id = 0;
    xdg_surface_id = 0;
    toplevel_id = 0;
    output_id = 0;
    output_version = 0;
    data_manager_id = 0;
    data_device_id = 0;
    decoration_manager_id = 0;
    decoration_id = 0;
    viewporter_id = 0;
    viewport_id = 0;
    subcompositor_id = 0;
    subsurface_id = 0;
    xdg_output_manager_id = 0;
    xdg_output_id = 0;
    activation_id = 0;
    activation_token_id = 0;
    fractional_manager_id = 0;
    fractional_id = 0;
    subsurface_surface_id = 0;
    subsurface_ok = 0;
    data_source_id = 0;
    data_offer_id = 0;
    data_mime_count = 0;
    seat_id = 0;
    seat_version = 0;
    keyboard_id = 0;
    pointer_id = 0;
    surface_id = 0;
    popup_surface_id = 0;
    popup_xdg_surface_id = 0;
    popup_id = 0;
    positioner_id = 0;
    popup_grab = 0;
    popup_committed = 0;
    pool_id = 0;
    buffer_id = 0;
    frame_id = 0;
    configure_serial = 1;
    verified = 0;
    commits = 0;
    focused = 0;
    surface_entered = 0;
    selection_changed = 0;
    wants_offer = 0;
}

void _start(void) {
    long server = sys3(198, 1, 1, 0);
    if (server < 0 || bind_path(server) < 0) {
        fail("wayland: bind failed\n", 21);
    }
    if (sys3(201 /* listen */, server, 4, 0) < 0) {
        fail("wayland: listen failed\n", 23);
    }

    long children[2];
    for (int index = 0; index < 2; index++) {
        long child = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
        if (child < 0) {
            fail("wayland: fork failed\n", 21);
        }
        if (child == 0) {
            static const char *source_argv[] = {"/sfs/wlclient", "source", 0};
            static const char *receiver_argv[] = {"/sfs/wlclient", "receiver", 0};
            const char **argv = index == 0 ? source_argv : receiver_argv;
            static const char *envp[] = {
                "XDG_RUNTIME_DIR=/tmp",
                "WAYLAND_DISPLAY=wayland-test",
                "LD_LIBRARY_PATH=/sfs",
                0,
            };
            sys6(221 /* execve */, (long)argv[0], (long)argv, (long)envp, 0, 0, 0);
            fail("wlreal: exec failed\n", 20);
        }
        children[index] = child;
    }

    int okay = 1;
    for (int index = 0; index < MAX_CLIENTS; index++) {
        long client = sys3(202 /* accept */, server, 0, 0);
        if (client < 0) {
            fail("wayland: accept failed\n", 23);
        }
        reset_client();
        clients[index].fd = client;
        clients[index].connected = 1;
        clients[index].next_server_id = 0xFF000000u;
        save_client(&clients[index]);
    }
    sys3(57, server, 0, 0);

    struct pollfd {
        int fd;
        short events;
        short revents;
    };
    int remaining = MAX_CLIENTS;
    while (remaining > 0) {
        struct pollfd ready[MAX_CLIENTS];
        int count = 0;
        for (int index = 0; index < MAX_CLIENTS; index++) {
            if (!clients[index].connected) {
                continue;
            }
            ready[count].fd = (int)clients[index].fd;
            ready[count].events = 1; /* POLLIN */
            ready[count].revents = 0;
            count += 1;
        }
        if (count == 0) {
            break;
        }
        if (sys6(73 /* ppoll */, (long)ready, count, 0, 0, 0, 0) <= 0) {
            continue;
        }
        for (int index = 0; index < MAX_CLIENTS; index++) {
            if (!clients[index].connected) {
                continue;
            }
            int slot = -1;
            for (int probe = 0; probe < count; probe++) {
                if (ready[probe].fd == (int)clients[index].fd) {
                    slot = probe;
                    break;
                }
            }
            if (slot < 0 || (ready[slot].revents & 1) == 0) {
                continue;
            }
            current_client = index;
            load_client(&clients[index]);
            int result = serve(clients[index].fd);
            save_client(&clients[index]);
            if (result != 0) {
                if (clipboard_owner == index) {
                    clipboard_owner = -1;
                    clipboard_mime_count = 0;
                }
                sys3(57, clients[index].fd, 0, 0);
                clients[index].connected = 0;
                remaining -= 1;
                if (result < 0) {
                    okay = 0;
                }
                continue;
            }
            if (selection_changed) {
                selection_changed = 0;
                clients[index].selection_changed = 0;
                wants_offer = 0;
                clients[index].wants_offer = 0;
                broadcast_clipboard();
            } else if (wants_offer) {
                wants_offer = 0;
                clients[index].wants_offer = 0;
                send_clipboard_offer(&clients[index]);
            }
        }
    }
    for (int index = 0; index < MAX_CLIENTS; index++) {
        if (clients[index].connected) {
            sys3(57, clients[index].fd, 0, 0);
            clients[index].connected = 0;
        }
        if (!clients[index].verified || clients[index].commits < 3 ||
            !clients[index].subsurface_ok) {
            okay = 0;
        }
    }

    for (int index = 0; index < 2; index++) {
        int status = 0;
        if (sys6(260 /* wait4 */, children[index], (long)&status, 0, 0, 0, 0) !=
            children[index]) {
            fail("wayland: wait failed\n", 21);
        }
        if ((status & 0x7f) != 0 || ((status >> 8) & 0xff) != 0) {
            fail("wayland: client failed\n", 23);
        }
    }
    if (!okay) {
        fail("wayland: pixels not verified\n", 30);
    }

    sys3(64, 1, (long)"libwayland 2 clients ok\n", 24);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
