/* DRM/KMS smoke test: enumerate the card, create a dumb buffer, bind it to a
 * framebuffer, flip it and wait for the vblank event. Prints one line per
 * step so a failed ioctl is obvious from the log. */
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
    if (value < 0) {
        print("-", 1);
    }
    do {
        digits[count++] = '0' + (number % 10);
        number /= 10;
    } while (number != 0);
    unsigned label_length = 0;
    while (label[label_length] != 0) {
        label_length += 1;
    }
    print(label, label_length);
    char out[24];
    int index = 0;
    while (count != 0) {
        out[index++] = digits[--count];
    }
    out[index++] = '\n';
    sys3(64, 1, (long)out, index);
}

struct drm_version {
    int major, minor, patchlevel, pad;
    unsigned long name_len;
    char *name;
    unsigned long date_len;
    char *date;
    unsigned long desc_len;
    char *desc;
};

struct drm_get_cap {
    unsigned long capability;
    unsigned long value;
};

struct drm_mode_card_res {
    unsigned long fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    unsigned count_fbs, count_crtcs, count_connectors, count_encoders;
    unsigned min_width, max_width, min_height, max_height;
};

struct drm_mode_modeinfo {
    unsigned clock;
    unsigned short hdisplay, hsync_start, hsync_end, htotal, hskew;
    unsigned short vdisplay, vsync_start, vsync_end, vtotal, vscan;
    unsigned vrefresh, flags, type;
    char name[32];
};

struct drm_mode_get_connector {
    unsigned long encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
    unsigned count_modes, count_props, count_encoders;
    unsigned encoder_id, connector_id, connector_type, connector_type_id;
    unsigned connection, mm_width, mm_height, subpixel, pad;
};

struct drm_mode_get_encoder {
    unsigned encoder_id, encoder_type, crtc_id, possible_crtcs, possible_clones;
};

struct drm_mode_crtc {
    unsigned long set_connectors_ptr;
    unsigned count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode_modeinfo mode;
};

struct drm_mode_create_dumb {
    unsigned height, width, bpp, flags, handle, pitch;
    unsigned long size;
};

struct drm_mode_map_dumb {
    unsigned handle, pad;
    unsigned long offset;
};

struct drm_mode_destroy_dumb {
    unsigned handle;
};

struct drm_mode_fb_cmd2 {
    unsigned fb_id, width, height, pixel_format, flags;
    unsigned handles[4], pitches[4], offsets[4];
    unsigned long modifier[4];
};

struct drm_mode_crtc_page_flip {
    unsigned crtc_id, fb_id, flags, reserved;
    unsigned long user_data;
};

#define DRM_IOCTL_VERSION 0xC0406400ul
#define DRM_IOCTL_GET_CAP 0xC010640Clu
#define DRM_IOCTL_MODE_GETRESOURCES 0xC04064A0ul
#define DRM_IOCTL_MODE_GETCRTC 0xC06864A1ul
#define DRM_IOCTL_MODE_GETCONNECTOR 0xC05064A7ul
#define DRM_IOCTL_MODE_GETENCODER 0xC01464A6ul
#define DRM_IOCTL_MODE_ADDFB2 0xC06864B8ul
#define DRM_IOCTL_MODE_PAGE_FLIP 0xC01864B0ul
#define DRM_IOCTL_MODE_CREATE_DUMB 0xC02064B2ul
#define DRM_IOCTL_MODE_MAP_DUMB 0xC01064B3ul

void _start(void) {
    long fd = sys3(56 /* openat */, -100, (long)"/dev/dri/card0", 2 /* O_RDWR */);
    if (fd < 0) {
        print("drm: no card0\n", 15);
        sys3(93, 1, 0, 0);
    }
    print("drm: card0 open\n", 16);

    struct drm_version version;
    for (unsigned i = 0; i < sizeof(version); i++) {
        ((char *)&version)[i] = 0;
    }
    long result = sys3(29 /* ioctl */, fd, DRM_IOCTL_VERSION, (long)&version);
    if (result < 0) {
        print("drm: VERSION failed\n", 20);
        sys3(93, 1, 0, 0);
    }
    print("drm: version ok\n", 17);

    struct drm_get_cap cap;
    cap.capability = 1; /* DRM_CAP_DUMB_BUFFER */
    cap.value = 0;
    if (sys3(29, fd, DRM_IOCTL_GET_CAP, (long)&cap) < 0 || cap.value != 1) {
        print("drm: no dumb buffers\n", 22);
        sys3(93, 1, 0, 0);
    }
    print("drm: dumb buffers ok\n", 22);

    struct drm_mode_card_res res;
    for (unsigned i = 0; i < sizeof(res); i++) {
        ((char *)&res)[i] = 0;
    }
    if (sys3(29, fd, DRM_IOCTL_MODE_GETRESOURCES, (long)&res) < 0) {
        print("drm: GETRESOURCES failed\n", 25);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: crtcs ", res.count_crtcs);
    print_number("drm: connectors ", res.count_connectors);

    unsigned crtc_ids[4];
    unsigned connector_ids[4];
    unsigned encoder_ids[4];
    res.crtc_id_ptr = (unsigned long)crtc_ids;
    res.connector_id_ptr = (unsigned long)connector_ids;
    res.encoder_id_ptr = (unsigned long)encoder_ids;
    res.count_crtcs = 4;
    res.count_connectors = 4;
    res.count_encoders = 4;
    if (sys3(29, fd, DRM_IOCTL_MODE_GETRESOURCES, (long)&res) < 0) {
        print("drm: GETRESOURCES ids failed\n", 29);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: crtc id ", crtc_ids[0]);
    print_number("drm: connector id ", connector_ids[0]);

    struct drm_mode_get_connector connector;
    for (unsigned i = 0; i < sizeof(connector); i++) {
        ((char *)&connector)[i] = 0;
    }
    connector.connector_id = connector_ids[0];
    if (sys3(29, fd, DRM_IOCTL_MODE_GETCONNECTOR, (long)&connector) < 0) {
        print("drm: GETCONNECTOR failed\n", 25);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: modes ", connector.count_modes);
    print_number("drm: connected ", connector.connection);

    struct drm_mode_modeinfo mode;
    for (unsigned i = 0; i < sizeof(mode); i++) {
        ((char *)&mode)[i] = 0;
    }
    connector.modes_ptr = (unsigned long)&mode;
    connector.count_modes = 1;
    if (sys3(29, fd, DRM_IOCTL_MODE_GETCONNECTOR, (long)&connector) < 0) {
        print("drm: GETCONNECTOR mode failed\n", 30);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: mode width ", mode.hdisplay);
    print_number("drm: mode height ", mode.vdisplay);

    struct drm_mode_get_encoder encoder;
    for (unsigned i = 0; i < sizeof(encoder); i++) {
        ((char *)&encoder)[i] = 0;
    }
    encoder.encoder_id = encoder_ids[0];
    if (sys3(29, fd, DRM_IOCTL_MODE_GETENCODER, (long)&encoder) < 0) {
        print("drm: GETENCODER failed\n", 23);
        sys3(93, 1, 0, 0);
    }
    print("drm: encoder ok\n", 16);

    struct drm_mode_crtc crtc;
    for (unsigned i = 0; i < sizeof(crtc); i++) {
        ((char *)&crtc)[i] = 0;
    }
    crtc.crtc_id = crtc_ids[0];
    if (sys3(29, fd, DRM_IOCTL_MODE_GETCRTC, (long)&crtc) < 0) {
        print("drm: GETCRTC failed\n", 20);
        sys3(93, 1, 0, 0);
    }
    print("drm: crtc ok\n", 13);

    unsigned width = mode.hdisplay;
    unsigned height = mode.vdisplay;
    struct drm_mode_create_dumb dumb;
    for (unsigned i = 0; i < sizeof(dumb); i++) {
        ((char *)&dumb)[i] = 0;
    }
    dumb.width = width;
    dumb.height = height;
    dumb.bpp = 32;
    if (sys3(29, fd, DRM_IOCTL_MODE_CREATE_DUMB, (long)&dumb) < 0) {
        print("drm: CREATE_DUMB failed\n", 24);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: handle ", dumb.handle);
    print_number("drm: pitch ", dumb.pitch);

    struct drm_mode_map_dumb map;
    for (unsigned i = 0; i < sizeof(map); i++) {
        ((char *)&map)[i] = 0;
    }
    map.handle = dumb.handle;
    if (sys3(29, fd, DRM_IOCTL_MODE_MAP_DUMB, (long)&map) < 0) {
        print("drm: MAP_DUMB failed\n", 21);
        sys3(93, 1, 0, 0);
    }
    long mapped = sys6(222 /* mmap */, 0, dumb.size, 3 /* RW */, 1 /* SHARED */,
                       fd, (long)map.offset);
    if (mapped <= 0) {
        print("drm: dumb mmap failed\n", 23);
        sys3(93, 1, 0, 0);
    }
    print("drm: dumb mapped\n", 17);

    struct drm_mode_fb_cmd2 fb;
    for (unsigned i = 0; i < sizeof(fb); i++) {
        ((char *)&fb)[i] = 0;
    }
    fb.width = width;
    fb.height = height;
    fb.pixel_format = 0x34325258; /* DRM_FORMAT_XRGB8888 */
    fb.handles[0] = dumb.handle;
    fb.pitches[0] = dumb.pitch;
    if (sys3(29, fd, DRM_IOCTL_MODE_ADDFB2, (long)&fb) < 0) {
        print("drm: ADDFB2 failed\n", 19);
        sys3(93, 1, 0, 0);
    }
    print_number("drm: fb ", fb.fb_id);

    /* Four quadrants: red, green, blue, white. */
    unsigned *pixels = (unsigned *)mapped;
    for (unsigned y = 0; y < height; y++) {
        for (unsigned x = 0; x < width; x++) {
            unsigned value;
            if (y < height / 2) {
                value = x < width / 2 ? 0x00FF0000u : 0x0000FF00u;
            } else {
                value = x < width / 2 ? 0x000000FFu : 0x00FFFFFFu;
            }
            pixels[y * (dumb.pitch / 4) + x] = value;
        }
    }
    __asm__ volatile("dsb sy" ::: "memory");

    struct drm_mode_crtc_page_flip flip;
    for (unsigned i = 0; i < sizeof(flip); i++) {
        ((char *)&flip)[i] = 0;
    }
    flip.crtc_id = crtc_ids[0];
    flip.fb_id = fb.fb_id;
    flip.user_data = 0x1234;
    if (sys3(29, fd, DRM_IOCTL_MODE_PAGE_FLIP, (long)&flip) < 0) {
        print("drm: PAGE_FLIP failed\n", 22);
        sys3(93, 1, 0, 0);
    }
    print("drm: flipped\n", 13);

    char event[32];
    long got = sys3(63 /* read */, fd, (long)event, sizeof(event));
    print_number("drm: event bytes ", got);
    if (got >= 8) {
        unsigned type = *(unsigned *)&event[0];
        print_number("drm: event type ", type);
    }
    print("drm test ok\n", 13);
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
