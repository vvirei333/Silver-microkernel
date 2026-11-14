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

static unsigned char *screen;
/* wl_surface of the application toplevel (in the app client's namespace).
   Only commits on this surface may become the app window: a client also
   commits cursor surfaces, and capturing one shrank the window to 24x24. */
static unsigned app_surface_id;
/* zwlr_foreign_toplevel_manager_v1 handle for the application window. Phosh
   folds the overview when it learns that a toplevel exists and lists it in the
   activity switcher; without the handle an app window floats over the app
   grid forever. The handle lives in Phosh's object namespace. */
static unsigned foreign_handle;
/* The compositor creates the handle resource, and Wayland reserves the ids
   whose high bit is set for server-created objects. A low id made libwayland
   abort Phosh's connection with EINVAL. */
#define WL_SERVER_ID_START 0xFF000000u
static unsigned foreign_handle_next = WL_SERVER_ID_START;
static char toplevel_title[96];
static char toplevel_app_id[96];
static unsigned char message[1024];
static unsigned long message_length;
/* GTK batches registry, shm, configure and surface traffic well past
  1 KiB. A short buffer used to drop the tail of a recvmsg, desynchronising
  the stream so later requests (notably wl_surface.commit) were never seen. */
#define INCOMING_SIZE 65536
/* The receive buffer belongs to the client being served: switching sockets
   must never splice one client's partial message onto another's bytes. */
static unsigned char incoming_fallback[INCOMING_SIZE];
static unsigned char *incoming = incoming_fallback;
static unsigned long incoming_pending;
static unsigned message_start;
static unsigned long incoming_length;
static unsigned long incoming_offset;
/* Descriptors passed with SCM_RIGHTS belong to the messages in stream order.
   libwayland flushes every pending message of a connection in one sendmsg, so
   several create_pool/add calls can arrive with several fds in a single cmsg
   array. Keeping only the first one silently dropped a pool and every buffer
   created from it, which is why windows drawn from such a pool (Phosh's home
   and its background) never reached the buffer table and never painted. */
#define MAX_INCOMING_FDS 8
static int incoming_fds[MAX_INCOMING_FDS];
static unsigned incoming_fd_count;
static int take_incoming_fd(void) {
    if (incoming_fd_count == 0) {
        return -1;
    }
    int fd = incoming_fds[0];
    for (unsigned index = 1; index < incoming_fd_count; index++) {
        incoming_fds[index - 1] = incoming_fds[index];
    }
    incoming_fd_count -= 1;
    return fd;
}

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

static void put_array(const void *bytes, unsigned length) {
    put_u32(length);
    const unsigned char *data = (const unsigned char *)bytes;
    for (unsigned index = 0; index < length; index++) {
        message[message_length++] = data[index];
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
    /* Wait up to 20 ms for a client message; a timeout lets `serve` pump the
       host input ring even when the client has nothing to say. */
    struct { long seconds; long nanoseconds; } timeout = { 0, 20000000 };
    struct { int fd; short events; short revents; } ready;
    ready.fd = (int)fd;
    ready.events = 1; /* POLLIN */
    ready.revents = 0;
    long polled = sys6(73 /* ppoll */, (long)&ready, 1, (long)&timeout, 0, 0, 0);
    if (polled == 0) {
        return -2;
    }
    if (polled < 0) {
        return -1;
    }
    struct iovec vector = {
        incoming + incoming_pending,
        INCOMING_SIZE - incoming_pending,
    };
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
    incoming_length = incoming_pending + (unsigned long)count;
    incoming_pending = 0;
    incoming_offset = 8;
    incoming_fd_count = 0;
    unsigned long control_offset = 0;
    while (control_offset + sizeof(struct cmsghdr) <= header.control_length) {
        struct cmsghdr *got =
            (struct cmsghdr *)(control.raw + control_offset);
        if (got->length < sizeof(struct cmsghdr) ||
            control_offset + got->length > header.control_length) {
            break;
        }
        if (got->level == 1 && got->kind == 1) {
            unsigned long bytes = got->length - sizeof(struct cmsghdr);
            unsigned count = (unsigned)(bytes / sizeof(int));
            int *fds = (int *)(control.raw + control_offset +
                               sizeof(struct cmsghdr));
            for (unsigned index = 0;
                 index < count && incoming_fd_count < MAX_INCOMING_FDS;
                 index++) {
                incoming_fds[incoming_fd_count++] = fds[index];
            }
        }
        control_offset += (got->length + 7ul) & ~7ul;
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
/* The client binds wl_shm more than once (GDK's registry and Phosh's own
 * registry share the display but allocate different proxy ids). Every proxy
 * must accept create_pool, not only the most recently bound one. */
#define MAX_SHM 8
static unsigned shm_ids[MAX_SHM];

static int is_shm(unsigned id) {
    if (id == 0) {
        return 0;
    }
    if (id == shm_id) {
        return 1;
    }
    for (unsigned index = 0; index < MAX_SHM; index++) {
        if (shm_ids[index] == id) {
            return 1;
        }
    }
    return 0;
}

static void shm_add(unsigned id) {
    if (id == 0) {
        return;
    }
    /* Check only the array: `is_shm` also matches the *current* `shm_id`,
       which would make this return before recording the id. */
    for (unsigned index = 0; index < MAX_SHM; index++) {
        if (shm_ids[index] == id) {
            return;
        }
    }
    for (unsigned index = 0; index < MAX_SHM; index++) {
        if (shm_ids[index] == 0) {
            shm_ids[index] = id;
            return;
        }
    }
}
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
static unsigned presentation_id;
static unsigned feedback_id;
static unsigned relative_manager_id;
static unsigned relative_pointer_id;
static unsigned constraints_id;
static unsigned locked_id;
static unsigned text_input_manager_id;
static unsigned text_input_id;
static unsigned layer_shell_id;
static unsigned layer_surface_id;
static unsigned layer_surface_surface;
static unsigned layer_width;
static unsigned layer_height;
static int layer_configured;
static int layer_acked;
#define MAX_LAYERS 16
/* The scanout is a 1024x768 surface. `OUTPUT_SCALE` is what the compositor
 * advertises through wl_output: at 1 the desktop is the full 1024x768, at 2
 * it is a 512x384 phone-sized desktop rendered with double-density buffers.
 * Every layer buffer is in physical pixels, so composition and hit-testing
 * share one 1024x768 space regardless of the advertised scale. */
#define OUTPUT_SCALE 1u
#define OUT_WIDTH 1024u
#define OUT_HEIGHT 768u
#define LOGICAL_WIDTH (OUT_WIDTH / OUTPUT_SCALE)
#define LOGICAL_HEIGHT (OUT_HEIGHT / OUTPUT_SCALE)
typedef struct {
    int used;
    unsigned surface;
    unsigned layer;
    unsigned anchor;
    int margin[4];
    unsigned width;
    unsigned height;
    unsigned stride;
    unsigned format;
    unsigned offset;
    int fd;
    /* Creation order within the client. Within one layer the surface created
       last is on top; commit frequency must not reorder the stack. */
    unsigned seq;
    unsigned keyboard_interactivity;
    /* zphoc_layer_shell_effects stacking: this surface must be below/above
       the given zwlr_layer_surface_v1 id. */
    unsigned layer_surface;
    unsigned below;
    unsigned above;
    /* zphoc_alpha_layer_surface_v1, 0..255. Phosh fades the panel backdrop
       between folded and unfolded states. */
    unsigned alpha;
    /* 1 once the client actually sent set_alpha. Until then the surface keeps
       the protocol's opaque default; Phosh's quick-settings backdrop only
       sends its alpha once the panel has been folded or unfolded, so at
       startup it would otherwise be an opaque sheet over the whole desktop. */
    unsigned char alpha_set;
    /* Own copy of the last committed pixels. GTK destroys the wl_shm pool as
       soon as the compositor releases the buffer, so re-reading the client
       fd on every composite made static layers (wallpaper, home) vanish as
       soon as another surface committed. */
    unsigned char *copy;
    unsigned copy_bytes;
    int copy_ready;
    /* Which Wayland client owns this surface. The layer scene is global, the
       protocol state stays per client. */
    int owner;
} LayerEntry;
static LayerEntry layer_entries[MAX_LAYERS];

/* Protocol state of each zwlr_layer_surface_v1. Phosh creates several (top
 * panel, background, home, ...), so a single "current layer surface" pair let
 * only the last one receive its configure. */
#define MAX_LAYER_SURFACES 16
typedef struct {
    int used;
    unsigned layer_surface;
    unsigned surface;
    unsigned layer;
    unsigned anchor;
    unsigned width;
    unsigned height;
    int margin[4];
    int configured;
    int acked;
    /* Set by set_size: the client changed the surface size and must receive a
       fresh configure on the next commit. Phosh expands the top panel to the
       full height when quick settings opens. */
    int pending;
    unsigned seq;
    unsigned keyboard_interactivity;
    unsigned below;
    unsigned above;
    unsigned alpha;
    unsigned char alpha_set;
} LayerSurface;
static LayerSurface layer_surfaces[MAX_LAYER_SURFACES];
static unsigned layer_surface_seq;

/* zphoc_layer_shell_effects_v1.get_stacked_layer_surface creates a helper
 * object per layer surface; Phosh then calls stack_below() on it. Keeping only
 * the newest helper id lost every relation but the last one. */
#define MAX_STACKED 16
typedef struct {
    unsigned id;
    unsigned layer_surface;
} StackedSlot;
static StackedSlot stacked_slots[MAX_STACKED];

static StackedSlot *stacked_add(unsigned id, unsigned layer_surface) {
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        if (stacked_slots[index].id == id) {
            stacked_slots[index].layer_surface = layer_surface;
            return &stacked_slots[index];
        }
    }
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        if (stacked_slots[index].id == 0) {
            stacked_slots[index].id = id;
            stacked_slots[index].layer_surface = layer_surface;
            return &stacked_slots[index];
        }
    }
    return 0;
}

static StackedSlot *stacked_find(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        if (stacked_slots[index].id == id) {
            return &stacked_slots[index];
        }
    }
    return 0;
}

/* zphoc_alpha_layer_surface_v1 helpers: one object per layer surface. */
#define MAX_ALPHA_SURFACES 8
typedef struct {
    int used;
    unsigned id;
    unsigned layer_surface;
    unsigned alpha; /* 0..255 */
} AlphaSlot;
static AlphaSlot alpha_slots[MAX_ALPHA_SURFACES];

static AlphaSlot *alpha_slot_add(unsigned id, unsigned layer_surface) {
    for (unsigned index = 0; index < MAX_ALPHA_SURFACES; index++) {
        if (alpha_slots[index].used && alpha_slots[index].layer_surface == layer_surface) {
            alpha_slots[index].id = id;
            return &alpha_slots[index];
        }
    }
    for (unsigned index = 0; index < MAX_ALPHA_SURFACES; index++) {
        if (!alpha_slots[index].used) {
            alpha_slots[index].used = 1;
            alpha_slots[index].id = id;
            alpha_slots[index].layer_surface = layer_surface;
            alpha_slots[index].alpha = 255;
            return &alpha_slots[index];
        }
    }
    return 0;
}

static AlphaSlot *alpha_slot_by_id(unsigned id) {
    for (unsigned index = 0; index < MAX_ALPHA_SURFACES; index++) {
        if (alpha_slots[index].used && alpha_slots[index].id == id) {
            return &alpha_slots[index];
        }
    }
    return 0;
}

static unsigned alpha_for_layer_surface(unsigned layer_surface) {
    for (unsigned index = 0; index < MAX_ALPHA_SURFACES; index++) {
        if (alpha_slots[index].used &&
            alpha_slots[index].layer_surface == layer_surface) {
            return alpha_slots[index].alpha;
        }
    }
    return 255;
}

static LayerSurface *layer_surface_by_id(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        if (layer_surfaces[index].used && layer_surfaces[index].layer_surface == id) {
            return &layer_surfaces[index];
        }
    }
    return 0;
}

static LayerSurface *layer_surface_by_surface(unsigned surface) {
    if (surface == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        if (layer_surfaces[index].used && layer_surfaces[index].surface == surface) {
            return &layer_surfaces[index];
        }
    }
    return 0;
}

static LayerSurface *layer_surface_add(unsigned id, unsigned surface) {
    LayerSurface *slot = layer_surface_by_id(id);
    if (slot != 0) {
        slot->surface = surface;
        return slot;
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        if (!layer_surfaces[index].used) {
            slot = &layer_surfaces[index];
            break;
        }
    }
    if (slot == 0) {
        slot = &layer_surfaces[0];
    }
    slot->used = 1;
    slot->layer_surface = id;
    slot->surface = surface;
    slot->layer = 0;
    slot->alpha = 255;
    slot->anchor = 0;
    slot->width = 0;
    slot->height = 0;
    for (unsigned index = 0; index < 4; index++) {
        slot->margin[index] = 0;
    }
    slot->configured = 0;
    slot->acked = 0;
    slot->pending = 1;
    slot->keyboard_interactivity = 0;
    slot->below = 0;
    slot->above = 0;
    slot->seq = ++layer_surface_seq;
    return slot;
}
static unsigned layer_anchor;
static unsigned layer_layer;
static int layer_margin[4];
static int text_input_enabled;
static unsigned text_input_serial;
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
/// The toplevel's wl_surface, kept separately from `surface_id` so a popup
/// (whose xdg_surface temporarily becomes the "current" one) cannot steal it.
static unsigned toplevel_surface_id;
static unsigned popup_surface_id;
/// Every wl_surface the client created, indexed by object id. GTK creates
/// several (toplevel, cursor, ...), so a single `surface_id` tracked only the
/// first one and missed the toplevel's attach/commit.
#define SURFACE_IDS 256
static unsigned char surface_ids[SURFACE_IDS];

/* wl_region objects and the per-surface input region. A client marks
 * decorative surfaces (panel shadows, backgrounds) with an empty input region;
 * ignoring that made those surfaces swallow pointer events meant for the
 * surface below them. Coordinates are surface-local logical units. */
#define MAX_REGIONS 64
typedef struct {
    unsigned id;
    unsigned rects;
    int x0;
    int y0;
    int x1;
    int y1;
} RegionSlot;
static RegionSlot regions[MAX_REGIONS];

static RegionSlot *region_find(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_REGIONS; index++) {
        if (regions[index].id == id) {
            return &regions[index];
        }
    }
    return 0;
}

static RegionSlot *region_add(unsigned id) {
    RegionSlot *slot = region_find(id);
    if (slot != 0) {
        slot->rects = 0;
        slot->x0 = slot->y0 = slot->x1 = slot->y1 = 0;
        return slot;
    }
    for (unsigned index = 0; index < MAX_REGIONS; index++) {
        if (regions[index].id == 0) {
            regions[index].id = id;
            regions[index].rects = 0;
            regions[index].x0 = regions[index].y0 = 0;
            regions[index].x1 = regions[index].y1 = 0;
            return &regions[index];
        }
    }
    return 0;
}

static void region_forget(unsigned id) {
    RegionSlot *slot = region_find(id);
    if (slot != 0) {
        slot->id = 0;
    }
}

/* wl_shm pools and buffers. A toolkit creates several pools (one per window
 * or layer surface) and buffers on older pools, so a single `pool_id` /
 * `buffer_id` pair lost both the create_buffer arguments and the release
 * target. The tables are part of ClientState; the globals below are the
 * currently loaded client's copy. */
#define MAX_POOLS 128
#define MAX_BUFFERS 512
typedef struct {
    unsigned id;
    int fd;
} PoolSlot;
typedef struct {
    unsigned id;
    int pool_fd;
    unsigned offset;
    unsigned width;
    unsigned height;
    unsigned stride;
    unsigned format;
    /* 1 for a dmabuf: the descriptor belongs to this buffer and is closed
       when it is destroyed. Shm buffers share their pool's descriptor. */
    unsigned char owned;
} BufferSlot;
/* zwp_linux_dmabuf_v1 params objects: each collects one plane and then turns
   into a wl_buffer. GTK creates one per frame. */
#define MAX_DMABUF_PARAMS 16
typedef struct {
    unsigned id;
    int fd;
    unsigned offset;
    unsigned stride;
    unsigned width;
    unsigned height;
    unsigned format;
    unsigned char has_plane;
} DmabufParams;
static DmabufParams dmabuf_params[MAX_DMABUF_PARAMS];
static unsigned dmabuf_ids[8];
static unsigned dmabuf_id;

/* zwp_linux_dmabuf_feedback_v1 objects created through
   get_default_feedback / get_surface_feedback. */
static unsigned dmabuf_feedbacks[16];

static int dmabuf_is_binding(unsigned object) {
    for (unsigned index = 0; index < 8; index++) {
        if (object != 0 && dmabuf_ids[index] == object) {
            return 1;
        }
    }
    return 0;
}

static int dmabuf_is_feedback(unsigned object) {
    for (unsigned index = 0; index < 16; index++) {
        if (object != 0 && dmabuf_feedbacks[index] == object) {
            return 1;
        }
    }
    return 0;
}

/* dev_t of /dev/dri/renderD128 (major 226, minor 128), the device every
   exported buffer belongs to. */
#define DRM_RENDER_RDEV 0xE280ull

#define DRM_FORMAT_ARGB8888 0x34325241u
#define DRM_FORMAT_XRGB8888 0x34325258u

/* Sends one complete dma-buf feedback batch: a format table with the window
   formats, the main device, one tranche for the render node and the final
   done event. GTK/Mesa use this both to choose buffer formats and to decide
   whether the compositor can import the buffers EGL allocates. */
static void send_dmabuf_feedback(long client, unsigned id) {
    /* Two formats, each with LINEAR and DRM_FORMAT_MOD_INVALID ((1 << 56)
       - 1). A driver without explicit-modifier allocation (virgl) needs to
       see INVALID in the list, otherwise Mesa refuses to allocate a window
       buffer at all. */
    unsigned char table[4][16];
    for (unsigned index = 0; index < 4; index++) {
        for (unsigned byte = 0; byte < 16; byte++) {
            table[index][byte] = 0;
        }
        unsigned format = (index / 2) == 0 ? DRM_FORMAT_ARGB8888
                                           : DRM_FORMAT_XRGB8888;
        table[index][0] = format & 0xff;
        table[index][1] = (format >> 8) & 0xff;
        table[index][2] = (format >> 16) & 0xff;
        table[index][3] = (format >> 24) & 0xff;
        if ((index % 2) == 1) {
            for (unsigned byte = 8; byte < 15; byte++) {
                table[index][byte] = 0xff;
            }
        }
    }
    long file = sys3(279 /* memfd_create */, (long)"werewolf-dmabuf", 0, 0);
    unsigned table_size = sizeof(table);
    long mapped = -1;
    if (file >= 0 && sys3(46 /* ftruncate */, file, (long)table_size, 0) == 0) {
        mapped = sys6(222 /* mmap */, 0, table_size, 3 /* RW */, 1 /* SHARED */,
                      file, 0);
    }
    if (mapped > 0) {
        unsigned char *target = (unsigned char *)mapped;
        for (unsigned index = 0; index < table_size; index++) {
            target[index] = ((const unsigned char *)table)[index];
        }
        begin(id, 1 /* zwp_linux_dmabuf_feedback_v1.format_table */);
        put_u32(table_size);
        finish();
        send_message(client, (int)file);
        sys3(215 /* munmap */, mapped, table_size, 0);
    }
    if (file >= 0) {
        sys3(57 /* close */, file, 0, 0);
    }

    begin(id, 2 /* main_device */);
    put_array(&(unsigned long){DRM_RENDER_RDEV}, sizeof(unsigned long));
    finish();
    send_message(client, -1);

    /* One tranche: everything can be scanned out on the render device. */
    begin(id, 4 /* tranche_target_device */);
    put_array(&(unsigned long){DRM_RENDER_RDEV}, sizeof(unsigned long));
    finish();
    send_message(client, -1);

    begin(id, 5 /* tranche_formats */);
    unsigned short indices[4] = {0, 1, 2, 3};
    put_array(indices, sizeof(indices));
    finish();
    send_message(client, -1);

    begin(id, 6 /* tranche_flags */);
    put_u32(0);
    finish();
    send_message(client, -1);

    begin(id, 3 /* tranche_done */);
    finish();
    send_message(client, -1);

    begin(id, 0 /* done */);
    finish();
    send_message(client, -1);
}

static DmabufParams *dmabuf_params_find(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_DMABUF_PARAMS; index++) {
        if (dmabuf_params[index].id == id) {
            return &dmabuf_params[index];
        }
    }
    return 0;
}

static DmabufParams *dmabuf_params_add(unsigned id) {
    DmabufParams *slot = dmabuf_params_find(id);
    if (slot != 0) {
        return slot;
    }
    for (unsigned index = 0; index < MAX_DMABUF_PARAMS; index++) {
        if (dmabuf_params[index].id == 0) {
            DmabufParams *fresh = &dmabuf_params[index];
            fresh->id = id;
            fresh->fd = -1;
            fresh->offset = 0;
            fresh->stride = 0;
            fresh->width = 0;
            fresh->height = 0;
            fresh->format = 0;
            fresh->has_plane = 0;
            return fresh;
        }
    }
    return 0;
}

/// Logs a dmabuf buffer's geometry (no stdio in this binary).
static void log_dmabuf_buffer(unsigned width, unsigned height, unsigned stride,
                              unsigned format) {
    static const char prefix[] = "gtk-serve: dmabuf ";
    char text[96];
    unsigned length = 0;
    for (unsigned index = 0; index < sizeof(prefix) - 1; index++) {
        text[length++] = prefix[index];
    }
    unsigned values[4] = {width, height, stride, format};
    for (unsigned value_index = 0; value_index < 4; value_index++) {
        unsigned value = values[value_index];
        char digits[12];
        unsigned count = 0;
        if (value == 0) {
            digits[count++] = '0';
        }
        while (value != 0) {
            digits[count++] = (char)('0' + value % 10);
            value /= 10;
        }
        while (count != 0) {
            text[length++] = digits[--count];
        }
        text[length++] = value_index == 3 ? '\n' : ' ';
    }
    sys3(64, 1, (long)text, length);
}

static PoolSlot pools[MAX_POOLS];
static BufferSlot buffers[MAX_BUFFERS];
static unsigned attached_buffers[SURFACE_IDS];
/* Set when the commit's attach supplied a buffer. A commit without attach
   keeps the previous buffer (the protocol), and must not re-release it:
   GDK's cairo renderer destroys the buffer's cairo surface on the first
   release, so a second one corrupts its refcount. */
static int attached_fresh[SURFACE_IDS];
/* Damage accumulated between commits: only that rectangle is re-read from the
   client's pool. GTK damages a few hundred pixels for a clock tick or a
   sheet edge, while the surface itself is a full-screen buffer. */
static int damage_box[SURFACE_IDS][4];
static unsigned char damage_valid[SURFACE_IDS];
/* 0 = no set_input_region (whole surface), 1 = region set (possibly empty). */
static unsigned char surface_input_set[SURFACE_IDS];
static unsigned char surface_input_empty[SURFACE_IDS];
static int surface_input_box[SURFACE_IDS][4];

static PoolSlot *pool_find(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        if (pools[index].id == id) {
            return &pools[index];
        }
    }
    return 0;
}

static PoolSlot *pool_add(unsigned id, int fd) {
    PoolSlot *free_slot = 0;
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        if (pools[index].id == id) {
            free_slot = &pools[index];
            break;
        }
        if (pools[index].id == 0 && free_slot == 0) {
            free_slot = &pools[index];
        }
    }
    if (free_slot == 0) {
        free_slot = &pools[0];
    }
    if (free_slot->fd >= 0 && free_slot->fd != fd) {
        sys3(57, free_slot->fd, 0, 0);
    }
    free_slot->id = id;
    free_slot->fd = fd;
    return free_slot;
}

static BufferSlot *buffer_find(unsigned id) {
    if (id == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        if (buffers[index].id == id) {
            return &buffers[index];
        }
    }
    return 0;
}

static BufferSlot *buffer_add(unsigned id) {
    BufferSlot *slot = buffer_find(id);
    if (slot != 0) {
        return slot;
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        if (buffers[index].id == 0) {
            buffers[index].id = id;
            return &buffers[index];
        }
    }
    /* A full table used to recycle slot 0, which silently handed a live
       buffer another buffer's fd/geometry. Refuse the buffer instead: the
       commit path then skips it rather than drawing the wrong pixels. */
    return 0;
}

static void buffer_forget(unsigned id) {
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        if (attached_buffers[index] == id) {
            attached_buffers[index] = 0;
        }
        if (attached_fresh[index] && attached_buffers[index] == 0) {
            attached_fresh[index] = 0;
        }
    }
    BufferSlot *slot = buffer_find(id);
    if (slot != 0) {
        if (slot->owned && slot->pool_fd >= 0) {
            sys3(57 /* close */, slot->pool_fd, 0, 0);
        }
        slot->id = 0;
        slot->pool_fd = -1;
        slot->owned = 0;
    }
}

static void pools_reset(void) {
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        if (pools[index].fd >= 0) {
            sys3(57, pools[index].fd, 0, 0);
        }
        pools[index].id = 0;
        pools[index].fd = -1;
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        buffers[index].id = 0;
        buffers[index].pool_fd = -1;
        buffers[index].offset = 0;
        buffers[index].width = 0;
        buffers[index].height = 0;
        buffers[index].stride = 0;
        buffers[index].format = 0;
        buffers[index].owned = 0;
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        attached_buffers[index] = 0;
    }
}

static int is_surface(unsigned id) {
    return id < SURFACE_IDS && surface_ids[id] != 0;
}
static unsigned popup_xdg_surface_id;
static unsigned popup_id;
static unsigned positioner_id;
/* Phosh-required globals: idle notifications, output power and phoc's
 * layer-shell effects. The effects are cosmetic here, but the requests must
 * be parsed so the client stream stays in sync. */
static unsigned idle_notifier_id;
static unsigned idle_notification_id;
static unsigned output_power_manager_id;
static unsigned output_power_id;
static unsigned layer_effects_id;
static unsigned draggable_id;
static unsigned alpha_id;
static unsigned stacked_id;
static unsigned foreign_toplevel_manager_id;
static unsigned gamma_manager_id;
static unsigned gamma_control_id;
/* phosh_private: the phosh<->compositor extension. Phosh calls
 * get_startup_tracker unconditionally and crashes on a NULL proxy, so the
 * request must at least be answered with a real object. */
static unsigned phosh_private_id;
static unsigned startup_tracker_id;
static unsigned keyboard_event_id;
static unsigned xdg_switcher_id;
static unsigned output_manager_id;
static unsigned output_config_id;
static int popup_grab;
static int popup_committed;
static unsigned pool_id;
static unsigned buffer_id;
static unsigned frame_id;
/* wl_surface.frame callbacks are completed on the surface's next commit, not
   on the request: completing them immediately made GTK render in a tight loop
   and burned the guest CPU. */
static unsigned frame_ids[SURFACE_IDS];
static int pool_fd = -1;
static int verified;
static unsigned commits;
static int buffer_width;
static int buffer_height;
static unsigned buffer_offset;
static unsigned buffer_stride;
static unsigned buffer_format;
/* Last xdg_surface.set_window_geometry() origin. A GTK toplevel buffer
   includes its client-side decoration margins, so the window content starts
   at this offset inside the buffer. */
static unsigned window_geometry_x;
static unsigned window_geometry_y;
static unsigned screen_seq;
/* A layer commit only marks the scene dirty: the main loop composes once per
   iteration. Phosh commits its background, the lockscreen and the panel in
   the same frame, and composing after each of them cost three full-frame
   blits per visible frame (~30 ms each under emulation). */
static int scene_dirty;
/* Union of the rectangles that changed since the last compose. A clock tick
   or a typed character only repaints its own box instead of the whole
   output, which is the difference between 27 ms and ~1 ms per frame. */
static int compose_box[4];
static int compose_full = 1;

static void compose_damage(int x0, int y0, int x1, int y1) {
    if (compose_full) {
        return;
    }
    if (compose_box[0] >= compose_box[2] || compose_box[1] >= compose_box[3]) {
        compose_box[0] = x0;
        compose_box[1] = y0;
        compose_box[2] = x1;
        compose_box[3] = y1;
        return;
    }
    if (x0 < compose_box[0]) compose_box[0] = x0;
    if (y0 < compose_box[1]) compose_box[1] = y0;
    if (x1 > compose_box[2]) compose_box[2] = x1;
    if (y1 > compose_box[3]) compose_box[3] = y1;
}
/* virtio-gpu scanout (`/dev/gpufb`). When present, every composed frame is
   also pushed through the GPU, so the device's own display head shows the
   desktop and a host screendump can prove the path. */
static unsigned char *gpufb;
static long gpufb_fd = -1;

/* Host input ring in the WIN1 header (tools/window-viewer/main.cpp). */
#define INPUT_RING 0x100
#define INPUT_SLOTS 64
#define INPUT_ENTRY 8
#define INPUT_HEAD 0x20
#define INPUT_TAIL 0x24
#define INPUT_KEY 1
#define INPUT_SCROLL 2
#define INPUT_CLICK 3
#define INPUT_MOVE 4
#define INPUT_RELEASE 5
static unsigned focus_surface;
static unsigned pointer_x;
static unsigned pointer_y;
/* Surface currently holding pointer/keyboard focus (0 = none). */
static unsigned pointer_target;
static unsigned keyboard_target;
/* Implicit grab: after a button press every event goes to that surface until
   the release, even when the pointer leaves its rectangle. Without it a drag
   that starts on the top panel is cancelled as soon as it moves below the
   panel's buffer. */
static unsigned pointer_grab;
#ifdef SERVE_ONLY
#endif
static unsigned attached_buffer; /* written by attach; release is deferred */
static int focused;
/* Per surface: a window only paints once it has entered an output (GDK treats
   a surface without `wl_surface.enter` as not on any output). One flag for
   the whole client meant only the first surface ever got the event, and
   Phosh's home/app-grid window kept committing empty frames. */
static unsigned char surface_entered[SURFACE_IDS];
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
    unsigned presentation_id;
    unsigned feedback_id;
    unsigned relative_manager_id;
    unsigned relative_pointer_id;
    unsigned constraints_id;
    unsigned locked_id;
    unsigned text_input_manager_id;
    unsigned text_input_id;
    unsigned layer_shell_id;
    unsigned layer_surface_id;
    unsigned layer_surface_surface;
    unsigned layer_width;
    unsigned layer_height;
    int layer_configured;
    int layer_acked;
    int text_input_enabled;
    unsigned text_input_serial;
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
    unsigned toplevel_surface_id;
    unsigned popup_surface_id;
    unsigned popup_xdg_surface_id;
    unsigned popup_id;
    unsigned positioner_id;
    unsigned window_geometry_x;
    unsigned window_geometry_y;
    unsigned idle_notifier_id;
    unsigned idle_notification_id;
    unsigned output_power_manager_id;
    unsigned output_power_id;
    unsigned layer_effects_id;
    unsigned draggable_id;
    unsigned alpha_id;
    unsigned stacked_id;
    unsigned foreign_toplevel_manager_id;
    unsigned gamma_manager_id;
    unsigned gamma_control_id;
    unsigned phosh_private_id;
    unsigned startup_tracker_id;
    unsigned keyboard_event_id;
    unsigned xdg_switcher_id;
    unsigned output_manager_id;
    unsigned output_config_id;
    int popup_grab;
    int popup_committed;
    unsigned pool_id;
    unsigned buffer_id;
    unsigned frame_id;
    unsigned frame_ids[SURFACE_IDS];
    int pool_fd;
    int verified;
    unsigned commits;
    int focused;
    unsigned char surface_entered[SURFACE_IDS];
    unsigned focus_surface;
    unsigned pointer_x;
    unsigned pointer_y;
    unsigned pointer_target;
    unsigned keyboard_target;
    unsigned pointer_grab;
    unsigned data_mime_count;
    char data_mimes[4][64];
    long fd;
    unsigned next_server_id;
    int connected;
    int selection_changed;
    int wants_offer;
    PoolSlot pools[MAX_POOLS];
    BufferSlot buffers[MAX_BUFFERS];
    unsigned attached_buffers[SURFACE_IDS];
    int damage_box[SURFACE_IDS][4];
    unsigned char damage_valid[SURFACE_IDS];
    unsigned char surface_input_set[SURFACE_IDS];
    unsigned char surface_input_empty[SURFACE_IDS];
    int surface_input_box[SURFACE_IDS][4];
    RegionSlot regions[MAX_REGIONS];
    LayerEntry layer_entries[MAX_LAYERS];
    LayerSurface layer_surfaces[MAX_LAYER_SURFACES];
    StackedSlot stacked_slots[MAX_STACKED];
    unsigned shm_ids[MAX_SHM];
    /* A partially received message must not be mixed with another client's
       bytes when the loop switches sockets. */
    unsigned char incoming_buffer[INCOMING_SIZE];
    unsigned long incoming_pending;
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
    X(presentation_id) X(feedback_id) \
    X(relative_manager_id) X(relative_pointer_id) \
    X(constraints_id) X(locked_id) \
    X(text_input_manager_id) X(text_input_id) \
    X(layer_shell_id) X(layer_surface_id) X(layer_surface_surface) \
    X(layer_width) X(layer_height) X(layer_configured) X(layer_acked) \
    X(text_input_enabled) X(text_input_serial) \
    X(seat_id) X(seat_version) X(keyboard_id) X(pointer_id) \
    X(configure_serial) X(surface_id) X(toplevel_surface_id) X(popup_surface_id) \
    X(window_geometry_x) X(window_geometry_y) \
    X(popup_xdg_surface_id) X(popup_id) X(positioner_id) \
    X(idle_notifier_id) X(idle_notification_id) \
    X(output_power_manager_id) X(output_power_id) \
    X(layer_effects_id) X(draggable_id) X(alpha_id) X(stacked_id) \
    X(foreign_toplevel_manager_id) X(gamma_manager_id) X(gamma_control_id) \
    X(phosh_private_id) X(startup_tracker_id) X(keyboard_event_id) X(xdg_switcher_id) \
    X(output_manager_id) X(output_config_id) \
    X(popup_grab) X(popup_committed) X(pool_id) X(buffer_id) X(frame_id) \
    X(pool_fd) X(verified) X(commits) X(focused) \
    X(focus_surface) X(pointer_x) X(pointer_y) X(pointer_target) \
    X(keyboard_target) X(pointer_grab) \
    X(data_mime_count) X(selection_changed) X(wants_offer)

#define MAX_CLIENTS 4
static ClientState clients[MAX_CLIENTS];

/* The newest xdg_toplevel, drawn over the layer scene. One window is enough
 * until the compositor grows a real window manager. */
#define APP_WIN_WIDTH 780
#define APP_WIN_HEIGHT 520
typedef struct {
    int used;
    int owner;
    unsigned surface;
    /* xdg_toplevel object in the app's namespace, for close requests. */
    unsigned toplevel;
    /* Top-left of the window content on the output. */
    int x;
    int y;
    unsigned width;
    unsigned height;
    unsigned stride;
    unsigned format;
    /* Top-left corner of the window content inside its buffer. */
    unsigned geometry_x;
    unsigned geometry_y;
    /* F1 moved the window out of the way (there is no window manager yet). */
    int hidden;
    unsigned char *copy;
    unsigned copy_bytes;
    int copy_ready;
} AppWindow;
static AppWindow app_window;
static int window_drag;
static int chrome_grab;
static int window_drag_dx;
static int window_drag_dy;
static int ui_client = -1; /* owns the layer scene (Phosh) */
static int frame_reported;
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
    incoming = client->incoming_buffer;
    incoming_pending = client->incoming_pending;
    copy_mimes(data_mimes, client->data_mimes);
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        pools[index] = client->pools[index];
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        buffers[index] = client->buffers[index];
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        attached_buffers[index] = client->attached_buffers[index];
        surface_entered[index] = client->surface_entered[index];
        damage_valid[index] = client->damage_valid[index];
        for (unsigned corner = 0; corner < 4; corner++) {
            damage_box[index][corner] = client->damage_box[index][corner];
        }
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        frame_ids[index] = client->frame_ids[index];
        surface_input_set[index] = client->surface_input_set[index];
        surface_input_empty[index] = client->surface_input_empty[index];
        for (unsigned corner = 0; corner < 4; corner++) {
            surface_input_box[index][corner] =
                client->surface_input_box[index][corner];
        }
    }
    for (unsigned index = 0; index < MAX_REGIONS; index++) {
        regions[index] = client->regions[index];
    }
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        stacked_slots[index] = client->stacked_slots[index];
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        layer_surfaces[index] = client->layer_surfaces[index];
    }
    for (unsigned index = 0; index < MAX_SHM; index++) {
        shm_ids[index] = client->shm_ids[index];
    }
}

static void save_client(ClientState *client) {
#define SAVE(field) client->field = field;
    CLIENT_SCALARS(SAVE)
#undef SAVE
    client->incoming_pending = incoming_pending;
    copy_mimes(client->data_mimes, data_mimes);
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        client->pools[index] = pools[index];
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        client->buffers[index] = buffers[index];
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        client->attached_buffers[index] = attached_buffers[index];
        client->surface_entered[index] = surface_entered[index];
        client->damage_valid[index] = damage_valid[index];
        for (unsigned corner = 0; corner < 4; corner++) {
            client->damage_box[index][corner] = damage_box[index][corner];
        }
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        client->frame_ids[index] = frame_ids[index];
        client->surface_input_set[index] = surface_input_set[index];
        client->surface_input_empty[index] = surface_input_empty[index];
        for (unsigned corner = 0; corner < 4; corner++) {
            client->surface_input_box[index][corner] =
                surface_input_box[index][corner];
        }
    }
    for (unsigned index = 0; index < MAX_REGIONS; index++) {
        client->regions[index] = regions[index];
    }
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        client->stacked_slots[index] = stacked_slots[index];
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        client->layer_surfaces[index] = layer_surfaces[index];
    }
    for (unsigned index = 0; index < MAX_SHM; index++) {
        client->shm_ids[index] = shm_ids[index];
    }
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

/* Monotonic milliseconds. GTK drives animations and gesture recognition from
 * these timestamps; sending 0 made every animation crawl. */
static unsigned now_ms(void) {
    struct {
        long seconds;
        long nanoseconds;
    } time;
    time.seconds = 0;
    time.nanoseconds = 0;
    if (sys3(113 /* clock_gettime */, 1 /* CLOCK_MONOTONIC */, (long)&time, 0) < 0) {
        return 0;
    }
    return (unsigned)((unsigned long)time.seconds * 1000u +
                      (unsigned long)(time.nanoseconds / 1000000));
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
    put_i32(APP_WIN_WIDTH);
    put_i32(APP_WIN_HEIGHT);
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





static LayerEntry *layer_entry_by_surface(unsigned surface) {
    if (surface == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_LAYERS; index++) {
        if (layer_entries[index].used &&
            layer_entries[index].surface == surface &&
            layer_entries[index].owner == current_client) {
            return &layer_entries[index];
        }
    }
    return 0;
}

static void layer_forget_surface(unsigned surface) {
    LayerEntry *entry = layer_entry_by_surface(surface);
    if (entry != 0) {
        entry->used = 0;
        /* The area it covered has to be repainted from the layers below. */
        compose_full = 1;
        scene_dirty = 1;
    }
}

/// Damage rectangle reported for the commit being processed (the whole
/// buffer when the client sent none), and clears it.
static void take_damage(int surface, int width, int height, int *x0, int *y0,
                        int *x1, int *y1) {
    if (surface >= 0 && surface < SURFACE_IDS && damage_valid[surface]) {
        *x0 = damage_box[surface][0];
        *y0 = damage_box[surface][1];
        *x1 = *x0 + damage_box[surface][2];
        *y1 = *y0 + damage_box[surface][3];
        damage_valid[surface] = 0;
        return;
    }
    *x0 = 0;
    *y0 = 0;
    *x1 = width;
    *y1 = height;
}

/// Snapshot the committed buffer into the layer's own memory. Called while
/// the client's pool fd is still alive (before wl_buffer.release). Only the
/// damaged rows/columns are re-read; the rest of the copy is already current.
/// Returns 1 when the region actually changed, 0 when the new frame is
/// identical to the stored one (a redraw that produced the same pixels: Phosh
/// redraws its lockscreen ~25 times a second, and skipping the compose for
/// those is most of the remaining win).
static int layer_store_pixels(LayerEntry *entry, int x0, int y0, int x1,
                              int y1, int *changed_x0, int *changed_y0,
                              int *changed_x1, int *changed_y1) {
    if (entry == 0 || buffer_width == 0 || buffer_height == 0) {
        return 0;
    }
    unsigned bytes = buffer_stride * (unsigned)buffer_height;
    if (bytes == 0) {
        return 0;
    }
    int fresh = 0;
    if (entry->copy == 0 || entry->copy_bytes < bytes) {
        if (entry->copy != 0) {
            sys3(215 /* munmap */, (long)entry->copy, entry->copy_bytes, 0);
            entry->copy = 0;
            entry->copy_bytes = 0;
        }
        long memory = sys6(222 /* mmap */, 0, bytes, 3 /* RW */,
                           0x22 /* MAP_PRIVATE|MAP_ANONYMOUS */, -1, 0);
        if (memory <= 0) {
            entry->copy_ready = 0;
            return 0;
        }
        entry->copy = (unsigned char *)memory;
        entry->copy_bytes = bytes;
        fresh = 1;
    }
    long mapped = sys6(222 /* mmap */, 0, buffer_offset + bytes,
                       1 /* read */, 1 /* MAP_SHARED */, pool_fd, 0);
    if (mapped <= 0) {
        entry->copy_ready = 0;
        return 0;
    }
    const unsigned char *source =
        (const unsigned char *)(mapped + buffer_offset);
    if (x0 < 0) x0 = 0;
    if (y0 < 0) y0 = 0;
    if (x1 > (int)buffer_width) x1 = (int)buffer_width;
    if (y1 > (int)buffer_height) y1 = (int)buffer_height;
    if (x0 >= x1 || y0 >= y1) {
        sys3(215 /* munmap */, mapped, buffer_offset + bytes, 0);
        entry->copy_ready = 1;
        return 0;
    }
    /* Find the rectangle that actually changed by comparing with the stored
       copy, 64 bits at a time. GTK damages its whole window on every redraw
       (a full-screen buffer) but usually changes only the clock or a hint,
       and repainting a megapixel frame for that is what kept the desktop at
       ~25 fps. */
    int cx0 = x1;
    int cy0 = y1;
    int cx1 = x0;
    int cy1 = y0;
    for (int row = y0; row < y1; row++) {
        const unsigned char *source_row =
            source + (unsigned)row * buffer_stride;
        const unsigned char *stored_row =
            entry->copy + (unsigned)row * buffer_stride;
        int column = x0;
        while (column + 2 <= x1) {
            unsigned long long a;
            unsigned long long b;
            __builtin_memcpy(&a, source_row + (unsigned)column * 4, 8);
            __builtin_memcpy(&b, stored_row + (unsigned)column * 4, 8);
            if (a != b) {
                if (column < cx0) cx0 = column;
                if (column + 2 > cx1) cx1 = column + 2;
                if (row < cy0) cy0 = row;
                if (row + 1 > cy1) cy1 = row + 1;
            }
            column += 2;
        }
        if (column < x1) {
            unsigned a;
            unsigned b;
            __builtin_memcpy(&a, source_row + (unsigned)column * 4, 4);
            __builtin_memcpy(&b, stored_row + (unsigned)column * 4, 4);
            if (a != b) {
                if (column < cx0) cx0 = column;
                if (column + 1 > cx1) cx1 = column + 1;
                if (row < cy0) cy0 = row;
                if (row + 1 > cy1) cy1 = row + 1;
            }
        }
    }
    if (fresh) {
        cx0 = x0;
        cy0 = y0;
        cx1 = x1;
        cy1 = y1;
    }
    if (cx0 >= cx1 || cy0 >= cy1) {
        sys3(215 /* munmap */, mapped, buffer_offset + bytes, 0);
        entry->copy_ready = 1;
        return 0;
    }
    for (int row = cy0; row < cy1; row++) {
        const unsigned char *source_row =
            source + (unsigned)row * buffer_stride;
        unsigned char *destination_row =
            entry->copy + (unsigned)row * buffer_stride;
        int column = cx0;
        while (column + 2 <= cx1) {
            unsigned long long value;
            __builtin_memcpy(&value, source_row + (unsigned)column * 4, 8);
            __builtin_memcpy(destination_row + (unsigned)column * 4,
                             &value, 8);
            column += 2;
        }
        if (column < cx1) {
            unsigned value;
            __builtin_memcpy(&value, source_row + (unsigned)column * 4, 4);
            __builtin_memcpy(destination_row + (unsigned)column * 4,
                             &value, 4);
        }
    }
    __asm__ volatile("dsb sy" ::: "memory");
    sys3(215 /* munmap */, mapped, buffer_offset + bytes, 0);
    entry->copy_ready = 1;
    *changed_x0 = cx0;
    *changed_y0 = cy0;
    *changed_x1 = cx1;
    *changed_y1 = cy1;
    return 1;
}
static void layer_record_commit(LayerSurface *slot) {
    if (slot == 0 || slot->surface == 0) {
        return;
    }
    LayerEntry *entry = layer_entry_by_surface(slot->surface);
    int appeared = 0;
    if (entry == 0) {
        appeared = 1;
        for (unsigned index = 0; index < MAX_LAYERS; index++) {
            if (!layer_entries[index].used) {
                entry = &layer_entries[index];
                break;
            }
        }
    }
    if (entry == 0) {
        return;
    }
    /* A surface that (re)appears, moves or changes layer order has to be
       repainted as a whole; the content diff only covers its own rectangle. */
    int geometry_changed = appeared ||
                           entry->width != (unsigned)buffer_width ||
                           entry->height != (unsigned)buffer_height ||
                           entry->layer != slot->layer ||
                           entry->anchor != slot->anchor;
    for (unsigned index = 0; index < 4 && !geometry_changed; index++) {
        if (entry->margin[index] != slot->margin[index] ||
            entry->below != slot->below || entry->above != slot->above) {
            geometry_changed = 1;
        }
    }
    if (geometry_changed) {
        compose_full = 1;
        scene_dirty = 1;
    }
    entry->used = 1;
    entry->surface = slot->surface;
    entry->owner = current_client;
    ui_client = current_client;
    entry->layer = slot->layer;
    entry->anchor = slot->anchor;
    for (unsigned index = 0; index < 4; index++) {
        entry->margin[index] = slot->margin[index];
    }
    entry->width = (unsigned)buffer_width;
    entry->height = (unsigned)buffer_height;
    entry->stride = buffer_stride;
    entry->format = buffer_format;
    entry->offset = buffer_offset;
    entry->fd = pool_fd;
    entry->seq = slot->seq;
    entry->keyboard_interactivity = slot->keyboard_interactivity;
    entry->layer_surface = slot->layer_surface;
    entry->below = slot->below;
    entry->above = slot->above;
    entry->alpha = slot->alpha;
    entry->alpha_set = slot->alpha_set;
}

/// Top-left corner of a layer surface in physical output coordinates. This is
/// the same anchor arithmetic `layer_compose` uses, so input and pixels agree.
static void layer_rect(const LayerEntry *entry, int *x, int *y) {
    /* anchor bits: 1 top, 2 bottom, 4 left, 8 right */
    if ((entry->anchor & 4) != 0 && (entry->anchor & 8) != 0) {
        *x = entry->margin[3];
    } else if ((entry->anchor & 8) != 0) {
        *x = (int)OUT_WIDTH - (int)entry->width - entry->margin[1];
    } else if ((entry->anchor & 4) != 0) {
        *x = entry->margin[3];
    } else {
        *x = ((int)OUT_WIDTH - (int)entry->width) / 2;
    }
    if ((entry->anchor & 1) != 0 && (entry->anchor & 2) != 0) {
        *y = entry->margin[0];
    } else if ((entry->anchor & 2) != 0) {
        *y = (int)OUT_HEIGHT - (int)entry->height - entry->margin[2];
    } else if ((entry->anchor & 1) != 0) {
        *y = entry->margin[0];
    } else {
        *y = ((int)OUT_HEIGHT - (int)entry->height) / 2;
    }
}

/// Whether `a` is stacked above `b`. Higher layers always win; inside a layer
/// the explicit zphoc stack_below()/stack_above() relation wins over creation
/// order. Phosh puts panel/lockscreen backgrounds below their content that
/// way, and without it those backgrounds take the pointer events.
static int layer_entry_above(const LayerEntry *a, const LayerEntry *b) {
    if (a->layer != b->layer) {
        return a->layer > b->layer;
    }
    if (a->below != 0 && a->below == b->layer_surface) {
        return 0;
    }
    if (b->below != 0 && b->below == a->layer_surface) {
        return 1;
    }
    if (a->above != 0 && a->above == b->layer_surface) {
        return 1;
    }
    if (b->above != 0 && b->above == a->layer_surface) {
        return 0;
    }
    /* Without an explicit relation the surface created *first* is on top:
       wlroots prepends new layer surfaces to the layer list and draws that
       list in order, so the oldest surface ends up above the newer ones.
       Phosh depends on it - the top panel is created before the home and its
       full-screen background, and must stay above them. */
    return a->seq < b->seq;
}

/// Topmost layer surface under a physical output coordinate, or 0. Higher
/// layer wins; within a layer the explicit stacking relation wins, then the
/// surface created last.
/* Draggable layer surfaces are needed by the hit test and the compose loop
   (a backdrop stacked below a folded sheet is invisible), so their state is
   declared here; the protocol handlers and the gesture are further down. */
#define MAX_DRAG_SURFACES 4
typedef struct {
    int used;
    unsigned id;            /* zphoc_draggable_layer_surface_v1 object */
    unsigned layer_surface; /* zwlr_layer_surface_v1 object */
    int folded;
    int unfolded;
    int current;
    unsigned threshold;     /* 8.8 fixed, 0.0 .. 1.0 */
    unsigned drag_mode;     /* 0 full, 1 handle, 2 none */
    unsigned drag_handle;
    unsigned exclusive;
    int state; /* 0 folded, 1 unfolded */
    int dragging;
    int start_margin;
    int start_pointer;
    int reported;
} DragSlot;
static DragSlot drag_slots[MAX_DRAG_SURFACES];
static DragSlot *drag_active;

/* Phosh leaves the alpha of the quick-settings backdrop at its opaque default
   until the panel has been folded or unfolded once; at startup the backdrop
   is therefore an opaque black sheet that hides the whole desktop. While the
   drag surface a backdrop is stacked under is folded, the backdrop is not
   visible either, so its alpha is zero unless the client set one itself. */
static unsigned layer_effective_alpha(const LayerEntry *entry) {
    if (entry->alpha_set) {
        return entry->alpha;
    }
    if (entry->below != 0) {
        for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
            DragSlot *slot = &drag_slots[index];
            if (slot->used && slot->layer_surface == entry->below) {
                if (slot->state == 0 && !slot->dragging) {
                    return 0;
                }
                break;
            }
        }
    }
    return entry->alpha;
}

static unsigned layer_hit_test(unsigned px, unsigned py) {
    LayerEntry *best = 0;
    for (unsigned index = 0; index < MAX_LAYERS; index++) {
        LayerEntry *entry = &layer_entries[index];
        if (!entry->used || entry->width == 0 || entry->height == 0 ||
            entry->fd < 0 || !entry->copy_ready) {
            continue;
        }
        /* A surface faded out with zphoc_alpha_layer_surface_v1 is invisible
           but still covers the screen. Phosh's "top-panel background" is a
           full-output surface at an overlay layer (the quick-settings
           backdrop); while it is transparent it must not swallow the pointer,
           otherwise the lockscreen never sees the unlock swipe. */
        if (layer_effective_alpha(entry) == 0) {
            continue;
        }
        int x;
        int y;
        layer_rect(entry, &x, &y);
        if ((int)px < x || (int)px >= x + (int)entry->width ||
            (int)py < y || (int)py >= y + (int)entry->height) {
            continue;
        }
        if (entry->surface < SURFACE_IDS && surface_input_set[entry->surface]) {
            if (surface_input_empty[entry->surface]) {
                continue;
            }
            /* The box is surface-local logical; the point is physical. */
            const int *box = surface_input_box[entry->surface];
            if ((int)px - x < box[0] * (int)OUTPUT_SCALE ||
                (int)px - x >= box[2] * (int)OUTPUT_SCALE ||
                (int)py - y < box[1] * (int)OUTPUT_SCALE ||
                (int)py - y >= box[3] * (int)OUTPUT_SCALE) {
                continue;
            }
        }
        if (best == 0 || layer_entry_above(entry, best)) {
            best = entry;
        }
    }
    return best != 0 ? best->surface : 0;
}

/// Topmost layer surface that asked for keyboard focus. An exclusive
/// (`keyboard_interactivity == 1`) surface wins regardless of the pointer;
/// `2` (on-demand) counts only while the pointer is inside it.
static unsigned keyboard_hit_test(void) {
    LayerEntry *best = 0;
    for (unsigned index = 0; index < MAX_LAYERS; index++) {
        LayerEntry *entry = &layer_entries[index];
        if (!entry->used || entry->width == 0 || entry->height == 0 ||
            entry->keyboard_interactivity == 0) {
            continue;
        }
        if (entry->keyboard_interactivity == 2) {
            int x;
            int y;
            layer_rect(entry, &x, &y);
            if ((int)pointer_x < x ||
                (int)pointer_x >= x + (int)entry->width ||
                (int)pointer_y < y ||
                (int)pointer_y >= y + (int)entry->height) {
                continue;
            }
        }
        if (best == 0 || layer_entry_above(entry, best)) {
            best = entry;
        }
    }
    return best != 0 ? best->surface : 0;
}

/* Pushes the composed pixels to the virtio-gpu and asks it to scan them out.
   The GPU buffer has no WIN1 header, so the frame is copied from the ivshmem
   pixel area, which stays the viewer's sink. */
static void gpu_present(unsigned width, unsigned height) {
    if (gpufb == 0 || gpufb_fd < 0 || screen == 0) {
        return;
    }
    const unsigned long long *source =
        (const unsigned long long *)(screen + 0x1000);
    unsigned long long *destination = (unsigned long long *)gpufb;
    unsigned count = (width * height) / 2;
    for (unsigned index = 0; index < count; index++) {
        destination[index] = source[index];
    }
    __asm__ volatile("dsb sy" ::: "memory");
    sys3(29 /* ioctl */, gpufb_fd, 0x5701 /* GPU_IOCTL_FLUSH */, 0);
}

/* The client draws its own title bar (CSD); the compositor overlays a close
   button in its top-right corner and moves the window when the title bar is
   dragged. Both are cheap per-pixel work, no toolkit involved. */
#define APP_CLOSE_SIZE 22
#define APP_TITLE_HEIGHT 34

static void app_window_content_rect(int *x, int *y, int *width, int *height) {
    int content_width = (int)app_window.width -
                        2 * (int)app_window.geometry_x;
    int content_height = (int)app_window.height -
                         2 * (int)app_window.geometry_y;
    if (content_width <= 0) content_width = (int)app_window.width;
    if (content_height <= 0) content_height = (int)app_window.height;
    *x = app_window.x;
    *y = app_window.y;
    *width = content_width;
    *height = content_height;
}

static void close_app_window(void) {
    if (app_window.used && app_window.toplevel != 0 &&
        app_window.owner >= 0 && app_window.owner < MAX_CLIENTS &&
        clients[app_window.owner].connected) {
        begin(app_window.toplevel, 8 /* xdg_toplevel.close */);
        finish();
        send_message(clients[app_window.owner].fd, -1);
    }
    app_window.hidden = 1;
    window_drag = 0;
    chrome_grab = 0;
#ifdef SERVE_ONLY
    scene_dirty = 1;
    compose_full = 1;
#endif
}

/// 1 when the compositor's window chrome (close button / title drag) took the
/// press, so it must not reach the application.
static int app_window_chrome(unsigned px, unsigned py) {
    if (!app_window.used || app_window.hidden || !app_window.copy_ready ||
        app_window.owner != current_client) {
        return 0;
    }
    int x, y, width, height;
    app_window_content_rect(&x, &y, &width, &height);
    if ((int)px < x || (int)px >= x + width ||
        (int)py < y || (int)py >= y + height) {
        return 0;
    }
    int close_x = x + width - APP_CLOSE_SIZE - 8;
    int close_y = y + 6;
    if ((int)px >= close_x && (int)px < close_x + APP_CLOSE_SIZE &&
        (int)py >= close_y && (int)py < close_y + APP_CLOSE_SIZE) {
        close_app_window();
        return 1;
    }
    if ((int)py < y + APP_TITLE_HEIGHT) {
        window_drag = 1;
        window_drag_dx = (int)px - app_window.x;
        window_drag_dy = (int)py - app_window.y;
        return 1;
    }
    return 0;
}

#ifdef SERVE_ONLY
static void layer_compose(void) {
    if (screen == 0) {
        return;
    }
    const unsigned out_width = OUT_WIDTH;
    const unsigned out_height = OUT_HEIGHT;
    unsigned *destination = (unsigned *)(screen + 0x1000);
    int clip_x0 = compose_full ? 0 : compose_box[0];
    int clip_y0 = compose_full ? 0 : compose_box[1];
    int clip_x1 = compose_full ? (int)out_width : compose_box[2];
    int clip_y1 = compose_full ? (int)out_height : compose_box[3];
    if (clip_x0 < 0) clip_x0 = 0;
    if (clip_y0 < 0) clip_y0 = 0;
    if (clip_x1 > (int)out_width) clip_x1 = (int)out_width;
    if (clip_y1 > (int)out_height) clip_y1 = (int)out_height;
    compose_full = 0;
    compose_box[0] = 0;
    compose_box[1] = 0;
    compose_box[2] = 0;
    compose_box[3] = 0;
    /* WIN1 seqlock, as in userspace/src/window_proto.rs: the frame counter is
       odd while the pixels are being painted and even once they are complete.
       A host viewer copies a frame only when it observes an even, changed
       counter and the counter is unchanged after the copy. Writing a constant
       odd value meant the window never updated at all. */
    screen_seq += 1;
    *(volatile unsigned *)(screen + 0x18) = screen_seq;
    __asm__ volatile("dsb sy" ::: "memory");
    /* Desktop backdrop. Phosh's own wallpaper lives in the home's background
       surface, which fades out when the overview folds, and its background
       manager needs a configured picture; without one the root would be a
       flat near-black. Paint a night-sky gradient instead: one multiply per
       row, then a plain fill, so it costs the same as the flat colour. */
    for (int row = clip_y0; row < clip_y1; row++) {
        unsigned *line = destination + (unsigned)row * out_width;
        unsigned t = (unsigned)row * 255u / (OUT_HEIGHT > 1 ? OUT_HEIGHT - 1 : 1);
        unsigned red = 10u + (28u - 10u) * t / 255u;
        unsigned green = 16u + (36u - 16u) * t / 255u;
        unsigned blue = 36u + (66u - 36u) * t / 255u;
        unsigned colour = 0xFF000000u | (blue << 16) | (green << 8) | red;
        for (int column = clip_x0; column < clip_x1; column++) {
            line[column] = colour;
        }
    }
    for (unsigned wanted = 0; wanted <= 3; wanted++) {
        unsigned order[MAX_LAYERS];
        unsigned count = 0;
        for (unsigned index = 0; index < MAX_LAYERS; index++) {
            LayerEntry *entry = &layer_entries[index];
            if (!entry->used || entry->layer != wanted || entry->width == 0 ||
                entry->height == 0 || entry->fd < 0) {
                continue;
            }
            order[count++] = index;
        }
        /* Insertion sort: order[0] is the bottom-most surface of the layer. */
        for (unsigned position = 1; position < count; position++) {
            unsigned key = order[position];
            unsigned slot = position;
            while (slot > 0 &&
                   layer_entry_above(&layer_entries[order[slot - 1]],
                                     &layer_entries[key])) {
                order[slot] = order[slot - 1];
                slot -= 1;
            }
            order[slot] = key;
        }
        for (unsigned position = 0; position < count; position++) {
            LayerEntry *entry = &layer_entries[order[position]];
            int x;
            int y;
            layer_rect(entry, &x, &y);
            const unsigned char *source;
            if (entry->copy_ready) {
                source = entry->copy;
            } else {
                continue;
            }
            const unsigned entry_alpha = layer_effective_alpha(entry);
            const int full_alpha = entry_alpha >= 255;
            const int force_opaque = entry->format == 1;
            /* Walk only the intersection of the layer and the damage box:
               iterating the whole layer and skipping out-of-clip pixels made
               a "partial" compose more expensive than a full one. */
            int start_x = x < clip_x0 ? clip_x0 - x : 0;
            int start_y = y < clip_y0 ? clip_y0 - y : 0;
            int end_x = (int)entry->width;
            int end_y = (int)entry->height;
            if (x + end_x > clip_x1) end_x = clip_x1 - x;
            if (y + end_y > clip_y1) end_y = clip_y1 - y;
            if (start_x < 0) start_x = 0;
            if (start_y < 0) start_y = 0;
            if (end_x <= start_x || end_y <= start_y) {
                continue;
            }
            for (int row = start_y; row < end_y; row++) {
                int out_y = y + row;
                const unsigned *source_row =
                    (const unsigned *)(source + (unsigned)row * entry->stride);
                if (full_alpha) {
                    /* Opaque layer: a straight word copy, no per-pixel
                       multiply or divide. */
                    unsigned *target_row =
                        destination + (unsigned)out_y * out_width;
                    for (int column = start_x; column < end_x; column++) {
                        unsigned pixel = source_row[column];
                        unsigned swizzled = (pixel & 0xFF00FF00u) |
                                            ((pixel & 0x00FF0000u) >> 16) |
                                            ((pixel & 0x000000FFu) << 16);
                        if (force_opaque) {
                            swizzled |= 0xFF000000u;
                        }
                        if ((swizzled >> 24) != 0) {
                            target_row[x + column] = swizzled | 0xFF000000u;
                        }
                    }
                    continue;
                }
                for (int column = start_x; column < end_x; column++) {
                    int out_x = x + column;
                    /* wl_shm ARGB8888 is premultiplied. Apply the surface
                       alpha from zphoc_alpha_layer_surface_v1 and blend over
                       the layers below; Phosh draws its home screen, panel
                       backdrop and sheets translucent, and writing them
                       straight to the sink turned every overlay black. */
                    unsigned pixel = source_row[column];
                    unsigned swizzled = (pixel & 0xFF00FF00u) |
                                        ((pixel & 0x00FF0000u) >> 16) |
                                        ((pixel & 0x000000FFu) << 16);
                    if (force_opaque) {
                        swizzled |= 0xFF000000u;
                    }
                    unsigned alpha = (swizzled >> 24) * entry_alpha / 255u;
                    if (alpha == 0) {
                        continue;
                    }
                    unsigned *target = &destination[(unsigned)out_y * out_width +
                                                    (unsigned)out_x];
                    if (alpha == 255) {
                        *target = swizzled | 0xFF000000u;
                        continue;
                    }
                    unsigned inverse = 255u - alpha;
                    unsigned below = *target;
                    unsigned red = (swizzled & 0xFFu) +
                                   ((below & 0xFFu) * inverse + 127u) / 255u;
                    unsigned green = ((swizzled >> 8) & 0xFFu) +
                                     (((below >> 8) & 0xFFu) * inverse + 127u) / 255u;
                    unsigned blue = ((swizzled >> 16) & 0xFFu) +
                                    (((below >> 16) & 0xFFu) * inverse + 127u) / 255u;
                    if (red > 255u) red = 255u;
                    if (green > 255u) green = 255u;
                    if (blue > 255u) blue = 255u;
                    *target = 0xFF000000u | (blue << 16) | (green << 8) | red;
                }
            }
        }
    }
    /* The newest toplevel (an application window) goes on top of the shell
       layers. Full-output only: there is no window manager yet. */
    if (app_window.used && !app_window.hidden && app_window.copy_ready &&
        app_window.width != 0) {
        /* The buffer carries the client-side decoration margins (a 780x520
           toplevel arrives as an 832x572 buffer), so the window is placed by
           its content rectangle and the margins land around it. */
        int origin_x = app_window.x - (int)app_window.geometry_x;
        int origin_y = app_window.y - (int)app_window.geometry_y;
        const int app_force_opaque = app_window.format == 1;
        int app_start_x = origin_x < clip_x0 ? clip_x0 - origin_x : 0;
        int app_start_y = origin_y < clip_y0 ? clip_y0 - origin_y : 0;
        int app_end_x = (int)app_window.width;
        int app_end_y = (int)app_window.height;
        if (origin_x + app_end_x > clip_x1) app_end_x = clip_x1 - origin_x;
        if (origin_y + app_end_y > clip_y1) app_end_y = clip_y1 - origin_y;
        if (app_start_x < 0) app_start_x = 0;
        if (app_start_y < 0) app_start_y = 0;
        for (int row = app_start_y; row < app_end_y; row++) {
            int out_y = origin_y + row;
            const unsigned *source_row = (const unsigned *)
                (app_window.copy + (unsigned)row * app_window.stride);
            for (int column = app_start_x; column < app_end_x; column++) {
                int out_x = origin_x + column;
                unsigned pixel = source_row[column];
                unsigned swizzled = (pixel & 0xFF00FF00u) |
                                    ((pixel & 0x00FF0000u) >> 16) |
                                    ((pixel & 0x000000FFu) << 16);
                if (app_force_opaque) {
                    swizzled |= 0xFF000000u;
                }
                unsigned alpha = swizzled >> 24;
                unsigned *target = &destination[(unsigned)out_y * out_width +
                                                (unsigned)out_x];
                if (alpha == 0) {
                    continue;
                }
                if (alpha == 255) {
                    *target = swizzled | 0xFF000000u;
                    continue;
                }
                unsigned inverse = 255u - alpha;
                unsigned below = *target;
                unsigned red = (swizzled & 0xFFu) +
                               ((below & 0xFFu) * inverse + 127u) / 255u;
                unsigned green = ((swizzled >> 8) & 0xFFu) +
                                 (((below >> 8) & 0xFFu) * inverse + 127u) / 255u;
                unsigned blue = ((swizzled >> 16) & 0xFFu) +
                                (((below >> 16) & 0xFFu) * inverse + 127u) / 255u;
                if (red > 255u) red = 255u;
                if (green > 255u) green = 255u;
                if (blue > 255u) blue = 255u;
                *target = 0xFF000000u | (blue << 16) | (green << 8) | red;
            }
        }
        /* Window outline and close button. The client's own title bar is at
           the top of its buffer, so the button lands in its right corner. */
        int chrome_x, chrome_y, chrome_w, chrome_h;
        app_window_content_rect(&chrome_x, &chrome_y, &chrome_w, &chrome_h);
        for (int row = chrome_y - 2; row < chrome_y + chrome_h + 2; row++) {
            if (row < clip_y0 || row >= clip_y1) continue;
            for (int column = chrome_x - 2; column < chrome_x + chrome_w + 2;
                 column++) {
                if (column < clip_x0 || column >= clip_x1) continue;
                if (row >= chrome_y && row < chrome_y + chrome_h &&
                    column >= chrome_x && column < chrome_x + chrome_w) {
                    continue;
                }
                destination[(unsigned)row * out_width + (unsigned)column] =
                    0xFF20242Eu;
            }
        }
        int button_x = chrome_x + chrome_w - APP_CLOSE_SIZE - 8;
        int button_y = chrome_y + 6;
        const int radius = APP_CLOSE_SIZE / 2 - 1;
        for (int row = 0; row < APP_CLOSE_SIZE; row++) {
            int out_y = button_y + row;
            if (out_y < clip_y0 || out_y >= clip_y1) continue;
            for (int column = 0; column < APP_CLOSE_SIZE; column++) {
                int out_x = button_x + column;
                if (out_x < clip_x0 || out_x >= clip_x1) continue;
                int dx = column - APP_CLOSE_SIZE / 2;
                int dy = row - APP_CLOSE_SIZE / 2;
                int ax = dx < 0 ? -dx : dx;
                int ay = dy < 0 ? -dy : dy;
                if (ax > radius || ay > radius) continue;
                unsigned colour = 0xFF3A3F4Bu;
                if (ax == ay && ax <= 5) colour = 0xFFD8DCE6u;
                destination[(unsigned)out_y * out_width + (unsigned)out_x] =
                    0xFF000000u | colour;
            }
        }
    }
    *(volatile unsigned *)(screen + 0x00) = 0x57494E31u;
    *(volatile unsigned *)(screen + 0x08) = out_width;
    *(volatile unsigned *)(screen + 0x0C) = out_height;
    *(volatile unsigned *)(screen + 0x10) = out_width * 4;
    *(volatile unsigned *)(screen + 0x14) = 1;
    /* STATE_READY, then the even sequence number that publishes the frame. */
    *(volatile unsigned *)(screen + 0x1C) = 1;
    *(volatile unsigned *)(screen + 0x2C) = 1;
    __asm__ volatile("dsb sy" ::: "memory");
    screen_seq += 1;
    *(volatile unsigned *)(screen + 0x18) = screen_seq;
    __asm__ volatile("dsb sy" ::: "memory");
    /* The ivshmem sink above is what the window viewer reads. Pushing the
       same frame through virtio-gpu costs another full-frame copy plus a
       device transfer (~5 ms per frame here), so refresh the device's own
       display head only occasionally. */
    static unsigned gpu_frames;
    gpu_frames += 1;
    if ((gpu_frames & 3u) == 0) {
        gpu_present(out_width, out_height);
    }

}
#endif

/* wl_pointer version 5 groups events into frames: a client that binds v5
 * waits for wl_pointer.frame before it applies motion/button/axis. Without it
 * GTK buffered every gesture forever. */
static void pointer_frame(long client) {
    if (seat_version < 5 || pointer_id == 0) {
        return;
    }
    begin(pointer_id, 5 /* frame */);
    finish();
    send_message(client, -1);
}

static void pointer_send_enter(long client, unsigned surface, int x, int y) {
    begin(pointer_id, 0 /* enter */);
    put_u32(configure_serial);
    put_u32(surface);
    put_i32(x << 8);
    put_i32(y << 8);
    finish();
    send_message(client, -1);
    pointer_frame(client);
}

static void pointer_send_leave(long client, unsigned surface) {
    begin(pointer_id, 1 /* leave */);
    put_u32(configure_serial);
    put_u32(surface);
    finish();
    send_message(client, -1);
    pointer_frame(client);
}

static void pointer_send_motion(long client, int x, int y) {
    begin(pointer_id, 2 /* motion */);
    put_u32(now_ms()); /* time */
    put_i32(x << 8);
    put_i32(y << 8);
    finish();
    send_message(client, -1);
    pointer_frame(client);
}

/* ---- zphoc_draggable_layer_surface_v1 -------------------------------- */
/* Phosh's top panel is a draggable layer surface: the compositor owns the
 * gesture, moves the surface with a margin and tells the client which end
 * position it settled on. Without this the panel starts (and stays) fully
 * unfolded, covering the lock screen with quick settings. */

static DragSlot *drag_slot_by_id(unsigned id) {
    for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
        if (drag_slots[index].used && drag_slots[index].id == id) {
            return &drag_slots[index];
        }
    }
    return 0;
}

static DragSlot *drag_slot_by_layer_surface(unsigned layer_surface) {
    if (layer_surface == 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
        if (drag_slots[index].used &&
            drag_slots[index].layer_surface == layer_surface) {
            return &drag_slots[index];
        }
    }
    return 0;
}

static DragSlot *drag_slot_add(unsigned id, unsigned layer_surface) {
    DragSlot *slot = drag_slot_by_layer_surface(layer_surface);
    if (slot == 0) {
        for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
            if (!drag_slots[index].used) {
                slot = &drag_slots[index];
                break;
            }
        }
    }
    if (slot == 0) {
        return 0;
    }
    slot->used = 1;
    slot->id = id;
    slot->layer_surface = layer_surface;
    slot->folded = 0;
    slot->unfolded = 0;
    slot->current = 0;
    slot->state = 0;
    slot->threshold = 256;
    slot->drag_mode = 0;
    slot->drag_handle = 0;
    slot->exclusive = 0;
    slot->dragging = 0;
    slot->reported = 0;
    return slot;
}

/// Which layer-surface margin the drag moves: 0 top, 1 right, 2 bottom,
/// 3 left. Matches the four three-edge anchors the protocol accepts.
static int drag_axis(unsigned anchor) {
    switch (anchor & 15u) {
    case 13: return 0; /* top|left|right */
    case 7:  return 1; /* top|bottom|right */
    case 14: return 2; /* bottom|left|right */
    case 11: return 3; /* top|bottom|left */
    default: return -1;
    }
}

static void drag_apply(unsigned layer_surface) {
    DragSlot *slot = drag_slot_by_layer_surface(layer_surface);
    LayerSurface *ls = layer_surface_by_id(layer_surface);
    if (slot == 0 || ls == 0) {
        return;
    }
    int axis = drag_axis(ls->anchor);
    if (axis < 0) {
        return;
    }
    LayerEntry *entry = layer_entry_by_surface(ls->surface);
    if (entry == 0 || !entry->used) {
        return;
    }
    entry->margin[axis] = slot->current * (int)OUTPUT_SCALE;
}

/// Recompose after the compositor moved a draggable surface. The plain
/// libwayland test build has no scanout, so this is a no-op there.
static void drag_recompose(void) {
#ifdef SERVE_ONLY
    scene_dirty = 1;
    compose_full = 1;
#endif
}

static void drag_notify(long client, DragSlot *slot, unsigned opcode,
                        unsigned value) {
    begin(slot->id, opcode);
    put_u32(value);
    finish();
    send_message(client, -1);
}

/// Starts a drag when the press is inside a draggable surface (or its drag
/// handle) and returns 1, so the press is consumed by the gesture instead of
/// being forwarded to the client.
/// 1 when this layer surface belongs to a drag slot (panel, home).
static int layer_has_drag_surface(unsigned surface) {
    for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
        DragSlot *slot = &drag_slots[index];
        if (!slot->used) {
            continue;
        }
        LayerSurface *ls = layer_surface_by_id(slot->layer_surface);
        if (ls != 0 && ls->surface == surface) {
            return 1;
        }
    }
    return 0;
}

static int drag_begin(long client, unsigned px, unsigned py) {
    (void)client;
    /* A draggable surface may only claim the press when it is the surface
       under the pointer. Phosh keeps the home (a drag surface) unfolded at
       full size underneath the lock screen; matching by rectangle alone made
       every press on the lock screen a home drag, so it could never be
       unlocked with the mouse. */
    unsigned under_pointer = layer_hit_test(px, py);
    for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
        DragSlot *slot = &drag_slots[index];
        if (!slot->used) {
            continue;
        }
        if (slot->drag_mode == 2) {
            continue;
        }
        LayerSurface *ls = layer_surface_by_id(slot->layer_surface);
        if (ls == 0) {
            continue;
        }
        LayerEntry *entry = layer_entry_by_surface(ls->surface);
        if (entry == 0 || !entry->used || entry->width == 0) {
            continue;
        }
        if (entry->owner != current_client) {
            continue;
        }
        if (ls->surface != under_pointer) {
            continue;
        }
        int axis = drag_axis(entry->anchor);
        if (axis < 0) {
            continue;
        }
        int x;
        int y;
        layer_rect(entry, &x, &y);
        if ((int)px < x || (int)px >= x + (int)entry->width ||
            (int)py < y || (int)py >= y + (int)entry->height) {
            continue;
        }
        slot->dragging = 1;
        slot->start_margin = slot->current;
        slot->start_pointer = (axis == 0 || axis == 2) ? (int)py : (int)px;
        slot->reported = 0;
        drag_active = slot;
        return 1;
    }
    return 0;
}

static void drag_motion(long client, unsigned px, unsigned py) {
    DragSlot *slot = drag_active;
    if (slot == 0) {
        return;
    }
    LayerSurface *ls = layer_surface_by_id(slot->layer_surface);
    if (ls == 0) {
        drag_active = 0;
        return;
    }
    int axis = drag_axis(ls->anchor);
    /* Top- and bottom-anchored surfaces move vertically, left/right ones
       horizontally. Using x for the top panel made every drag a no-op. */
    int pointer = (axis == 0 || axis == 2) ? (int)py : (int)px;
    int delta = (pointer - slot->start_pointer) / (int)OUTPUT_SCALE;
    int margin = slot->start_margin + delta;
    int low = slot->folded < slot->unfolded ? slot->folded : slot->unfolded;
    int high = slot->folded < slot->unfolded ? slot->unfolded : slot->folded;
    if (margin < low) margin = low;
    if (margin > high) margin = high;
    if (margin == slot->current && slot->reported) {
        return;
    }
    slot->current = margin;
    drag_apply(slot->layer_surface);
    drag_recompose();
    drag_notify(client, slot, 0 /* dragged */, (unsigned)margin);
    slot->reported = 1;
}

static void drag_finish(long client) {
    DragSlot *slot = drag_active;
    if (slot == 0) {
        return;
    }
    drag_active = 0;
    slot->dragging = 0;
    int span = slot->unfolded - slot->folded;
    int progress = slot->current - slot->folded;
    int unfolded = 1;
    if (span != 0 && progress * 256 < span * (int)slot->threshold) {
        unfolded = 0;
    }
    slot->state = unfolded;
    slot->current = unfolded ? slot->unfolded : slot->folded;
    drag_apply(slot->layer_surface);
    drag_recompose();
    drag_notify(client, slot, 1 /* drag_end */, (unsigned)unfolded);
}

/// Route the pointer at physical output (px, py) to the topmost layer surface
/// under it, falling back to the focused toplevel. `motion` sends a motion
/// event when the surface does not change.
static void pointer_route(long client, unsigned px, unsigned py, int motion) {
    if (pointer_id == 0) {
        return;
    }
    unsigned target = pointer_grab;
    int local_x;
    int local_y;
    if (target == 0) {
        target = layer_hit_test(px, py);
        if (target != 0) {
            /* Surface identifiers live in the owning client's namespace. An
               id that happens to be the shell's layer is a random object (or
               nothing at all) for an application, and libwayland rejects such
               an event with "unknown object" and kills the connection. */
            LayerEntry *hit = layer_entry_by_surface(target);
            if (hit == 0 || hit->owner != current_client) {
                target = 0;
            }
        }
    }
    int from_app = 0;
    int app_left = app_window.x - (int)app_window.geometry_x;
    int app_top = app_window.y - (int)app_window.geometry_y;
    if (target == 0 && app_window.used && !app_window.hidden &&
        app_window.copy_ready && app_window.owner == current_client &&
        (int)px >= app_left && (int)px < app_left + (int)app_window.width &&
        (int)py >= app_top && (int)py < app_top + (int)app_window.height) {
        /* The application window is drawn over the whole scene, so it is the
           topmost surface under the pointer for its own client. */
        target = app_window.surface;
        from_app = 1;
    }
    if (target != 0) {
        LayerEntry *entry = layer_entry_by_surface(target);
        int x = 0;
        int y = 0;
        if (from_app) {
            x = app_window.x - (int)app_window.geometry_x;
            y = app_window.y - (int)app_window.geometry_y;
        } else if (entry != 0) {
            layer_rect(entry, &x, &y);
        }
        local_x = ((int)px - x) / (int)OUTPUT_SCALE;
        local_y = ((int)py - y) / (int)OUTPUT_SCALE;
    } else {
        /* Fall back to the xdg toplevel only when this client has no visible
           layer surface at all; otherwise a miss means "no surface here". */
        int any_layer = 0;
        for (unsigned index = 0; index < MAX_LAYERS; index++) {
            if (layer_entries[index].used && layer_entries[index].width != 0) {
                any_layer = 1;
                break;
            }
        }
        if (any_layer) {
            return;
        }
        target = focus_surface;
        local_x = (int)px / (int)OUTPUT_SCALE;
        local_y = (int)py / (int)OUTPUT_SCALE;
    }
    if (target == 0) {
        return;
    }
    if (target != pointer_target) {
        if (pointer_target != 0) {
            pointer_send_leave(client, pointer_target);
        }
        pointer_send_enter(client, target, local_x, local_y);
        pointer_target = target;
    } else if (motion) {
        pointer_send_motion(client, local_x, local_y);
    }
}

/// Give keyboard focus to the topmost surface that asked for it. Phosh's
/// lockscreen requests exclusive keyboard interactivity, so it wins over the
/// background and panel even though those commit later.
static void keyboard_route(long client) {
    if (keyboard_id == 0) {
        return;
    }
    unsigned target = keyboard_hit_test();
    if (target != 0) {
        /* Same namespace rule as the pointer: never hand a client a focus
           target that belongs to the shell or another application. */
        LayerEntry *hit = layer_entry_by_surface(target);
        if (hit == 0 || hit->owner != current_client) {
            target = 0;
        }
    }
    if (target == 0) {
        target = focus_surface;
    }
    if (target == 0 || target == keyboard_target) {
        return;
    }
    if (keyboard_target != 0) {
        begin(keyboard_id, 2 /* leave */);
        put_u32(configure_serial);
        put_u32(keyboard_target);
        finish();
        send_message(client, -1);
    }
    begin(keyboard_id, 1 /* enter */);
    put_u32(configure_serial);
    put_u32(target);
    put_u32(0); /* keys array, empty */
    finish();
    send_message(client, -1);
    keyboard_target = target;
}

/* ---- phosh_private_keyboard_event: global accelerators ----------------- */
/* Phosh asks the compositor to grab global shortcuts (Super for the overview,
 * Print for screenshots, ...). The compositor owns the keyboard, so without
 * these grabs the shell never sees them. */
#define MAX_GRABS 16
typedef struct {
    int used;
    unsigned action;
    char accelerator[64];
} GrabSlot;
static GrabSlot grabs[MAX_GRABS];
static unsigned next_action_id = 1;

static unsigned grab_keycode(const char *accelerator) {
    const char *name = accelerator;
    while (*name == '<') {
        const char *end = name;
        while (*end != '\0' && *end != '>') end++;
        if (*end != '>') break;
        name = end + 1;
    }
    if (text_equal(name, "Super_L") || text_equal(name, "Super")) return 125;
    if (text_equal(name, "Super_R")) return 126;
    if (text_equal(name, "Print")) return 210;
    if (name[0] == 'F' && name[1] >= '1' && name[1] <= '9' && name[2] == '\0')
        return 58 + (unsigned)(name[1] - '0');
    if (name[0] == 'F' && name[1] == '1' && name[2] >= '0' && name[2] <= '2')
        return 87 + (unsigned)(name[2] - '0');
    if (name[1] == '\0' && name[0] >= 'a' && name[0] <= 'z') {
        static const unsigned letters[26] = {
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
            49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44};
        return letters[(unsigned)(name[0] - 'a')];
    }
    return 0;
}

static GrabSlot *grab_find_by_accelerator(const char *accelerator) {
    for (unsigned index = 0; index < MAX_GRABS; index++) {
        if (grabs[index].used &&
            text_equal(grabs[index].accelerator, accelerator)) {
            return &grabs[index];
        }
    }
    return 0;
}

static GrabSlot *grab_add(const char *accelerator) {
    GrabSlot *slot = grab_find_by_accelerator(accelerator);
    if (slot != 0) {
        return 0;
    }
    for (unsigned index = 0; index < MAX_GRABS; index++) {
        if (!grabs[index].used) {
            grabs[index].used = 1;
            grabs[index].action = next_action_id++;
            unsigned length = 0;
            while (accelerator[length] != '\0' &&
                   length + 1 < sizeof(grabs[index].accelerator)) {
                grabs[index].accelerator[length] = accelerator[length];
                length++;
            }
            grabs[index].accelerator[length] = '\0';
            return &grabs[index];
        }
    }
    return 0;
}

static void grab_success(long client, const GrabSlot *slot) {
    begin(keyboard_event_id, 2 /* grab_success_event */);
    put_string(slot->accelerator);
    put_u32(slot->action);
    finish();
    send_message(client, -1);
}

/// Forward a pressed key to the client if it matches a grabbed accelerator.
static void grab_activate(long client, unsigned keycode) {
    if (keyboard_event_id == 0) {
        return;
    }
    for (unsigned index = 0; index < MAX_GRABS; index++) {
        if (!grabs[index].used) {
            continue;
        }
        unsigned wanted = grab_keycode(grabs[index].accelerator);
        if (wanted != 0 && wanted == keycode) {
            begin(keyboard_event_id, 0 /* accelerator_activated_event */);
            put_u32(grabs[index].action);
            put_u32(now_ms());
            finish();
            send_message(client, -1);
        }
    }
}

static void launch_terminal(void) {
    if (app_window.used) {
        return;
    }
    long child = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (child != 0) {
        return;
    }
    static const char *argv[] = {"/sfs/term", 0};
    static const char *envp[] = {
        "XDG_RUNTIME_DIR=/tmp", "HOME=/tmp", "XKB_CONFIG_ROOT=/sfs/xkb",
        "WAYLAND_DISPLAY=wayland-test", "GDK_BACKEND=wayland",
        "LD_LIBRARY_PATH=/sfs",
        "XDG_DATA_DIRS=/tmp/xdg/share:/sfs/share:/sfs/usr/share",
        "GSETTINGS_SCHEMA_DIR=/sfs", "FONTCONFIG_FILE=/sfs/fonts.conf",
        "GSK_RENDERER=cairo", "GTK_A11Y=none",
        "XDG_CURRENT_DESKTOP=Phosh", "XDG_SESSION_TYPE=wayland",
        "XDG_SESSION_DESKTOP=phosh",
        /* wl_cursor_theme_load() only looks at XCURSOR_PATH (not
           XDG_DATA_DIRS); without a theme every cursor lookup failed and
           GDK rescanned every icon directory on each pointer enter. */
        "XCURSOR_THEME=default", "XCURSOR_PATH=/sfs/share/icons",
        /* No accessibility bus is running; loading atk-bridge makes GTK block
           for tens of seconds on the D-Bus activation timeout. */
        "NO_AT_BRIDGE=1",
        /* Terminal frames are wl_shm; without this GDK gives the window a GL
           context, which makes Mesa map llvmpipe and both libLLVM copies. */
        "GDK_GL=disable", "GDK_DEBUG=gl-disable", 0,
    };
    sys6(221 /* execve */, (long)argv[0], (long)argv, (long)envp, 0, 0, 0);
    sys3(93 /* exit */, 0, 0, 0);
}

#ifdef SERVE_ONLY
static void layer_compose(void);
#endif

/// Shows/hides the newest toplevel; launches the terminal when there is none.
static void toggle_app_window(void) {
    if (!app_window.used) {
        static const char msg[] = "gtk-serve: app launch\n";
        sys3(64, 1, (long)msg, sizeof(msg) - 1);
        launch_terminal();
        return;
    }
    app_window.hidden = !app_window.hidden;
    window_drag = 0;
    chrome_grab = 0;
    {
        static const char hide[] = "gtk-serve: app hide\n";
        static const char show[] = "gtk-serve: app show\n";
        const char *msg = app_window.hidden ? hide : show;
        unsigned length = app_window.hidden ? sizeof(hide) - 1 : sizeof(show) - 1;
        sys3(64, 1, (long)msg, length);
    }
#ifdef SERVE_ONLY
    scene_dirty = 1;
    compose_full = 1;
#endif
}

/* The host viewer sends printable keys as Unicode code points (case kept) and
   the rest as the small values from userspace/src/window_proto.rs, while the
   compositor speaks Linux evdev keycodes. `shift` reports whether a US-layout
   shift is needed for the character. */
static unsigned keycode_for(unsigned key, unsigned *shift) {
    static const unsigned char letters[26] = {
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
        49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44};
    *shift = 0;
    if (key >= 'a' && key <= 'z') return letters[key - 'a'];
    if (key >= 'A' && key <= 'Z') {
        *shift = 1;
        return letters[key - 'A'];
    }
    if (key >= '1' && key <= '9') return 2 + (key - '1');
    if (key == '0') return 11;
    switch (key) {
    case ' ': return 57;
    case '\t': return 15;
    case '\n': return 28;
    case 8:
    case 127: return 14;
    case '-': return 12;
    case '=': return 13;
    case '[': return 26;
    case ']': return 27;
    case ';': return 39;
    case '\'': return 40;
    case '`': return 41;
    case '\\': return 43;
    case ',': return 51;
    case '.': return 52;
    case '/': return 53;
    case '!': *shift = 1; return 2;
    case '@': *shift = 1; return 3;
    case '#': *shift = 1; return 4;
    case '$': *shift = 1; return 5;
    case '%': *shift = 1; return 6;
    case '^': *shift = 1; return 7;
    case '&': *shift = 1; return 8;
    case '*': *shift = 1; return 9;
    case '(': *shift = 1; return 10;
    case ')': *shift = 1; return 11;
    case '_': *shift = 1; return 12;
    case '+': *shift = 1; return 13;
    case '{': *shift = 1; return 26;
    case '}': *shift = 1; return 27;
    case ':': *shift = 1; return 39;
    case '"': *shift = 1; return 40;
    case '~': *shift = 1; return 41;
    case '|': *shift = 1; return 43;
    case '<': *shift = 1; return 51;
    case '>': *shift = 1; return 52;
    case '?': *shift = 1; return 53;
    /* KEY_UP..KEY_FORM from window_proto.rs. */
    case 1: return 103;
    case 2: return 108;
    case 3: return 104;
    case 4: return 109;
    case 5: return 102;
    case 6: return 107;
    case 11: return 14;
    case 12: return 1;
    case 13: return 38;
    case 17: return 16;
    case 18: return 20;
    case 19: return 17;
    case 21: return 33;
    default: return 0;
    }
}

/// Tells Phosh about the application window (see `foreign_handle`).
static void foreign_toplevel_announce(void) {
    /* The manager object lives in Phosh's namespace; the globals hold the
       state of the client whose message is being processed (the app). */
    unsigned manager = (ui_client >= 0 && ui_client < MAX_CLIENTS)
                           ? clients[ui_client].foreign_toplevel_manager_id
                           : 0;
    if (manager == 0 || ui_client < 0 || ui_client >= MAX_CLIENTS ||
        !clients[ui_client].connected) {
        return;
    }
    long phosh = clients[ui_client].fd;
    foreign_handle = foreign_handle_next;
    foreign_handle_next += 4;
    begin(manager, 0 /* toplevel(new_id) */);
    put_u32(foreign_handle);
    finish();
    send_message(phosh, -1);
    begin(foreign_handle, 0 /* title */);
    put_string(toplevel_title[0] != 0 ? toplevel_title : "Application");
    finish();
    send_message(phosh, -1);
    begin(foreign_handle, 1 /* app_id */);
    put_string(toplevel_app_id);
    finish();
    send_message(phosh, -1);
    begin(foreign_handle, 2 /* output_enter */);
    put_u32(clients[ui_client].output_id);
    finish();
    send_message(phosh, -1);
    begin(foreign_handle, 5 /* done */);
    finish();
    send_message(phosh, -1);
}

static void foreign_toplevel_closed(void) {
    if (foreign_handle == 0 || ui_client < 0 || ui_client >= MAX_CLIENTS ||
        !clients[ui_client].connected) {
        foreign_handle = 0;
        return;
    }
    begin(foreign_handle, 6 /* closed */);
    finish();
    send_message(clients[ui_client].fd, -1);
    foreign_handle = 0;
}

/// `wl_keyboard.modifiers`: bit 0 is shift.
static void keyboard_send_modifiers(long client, unsigned depressed) {
    if (keyboard_id == 0) {
        return;
    }
    begin(keyboard_id, 4 /* modifiers */);
    put_u32(configure_serial);
    put_u32(depressed);
    put_u32(0); /* latched */
    put_u32(0); /* locked */
    put_u32(0); /* group */
    finish();
    send_message(client, -1);
}

/* Dragging the lock screen up. libhandy's unlock is a touch-only swipe and
   ignores pointer sequences, so the compositor performs the gesture itself:
   the layer surface follows the pointer, and releasing after a long enough
   drag sends the key press that unlocks (require-unlock is off). */
static int lock_drag;
static unsigned lock_drag_surface;
static int lock_drag_start;
static int lock_drag_offset;

static int lock_drag_begin(unsigned px, unsigned py) {
    unsigned surface = layer_hit_test(px, py);
    if (surface == 0 || layer_has_drag_surface(surface)) {
        return 0;
    }
    if (app_window.used && !app_window.hidden &&
        surface == app_window.surface) {
        return 0;
    }
    lock_drag = 1;
    lock_drag_surface = surface;
    lock_drag_start = (int)py;
    lock_drag_offset = 0;
    return 1;
}

static void lock_drag_move(unsigned py) {
    if (!lock_drag) {
        return;
    }
    int offset = (int)py - lock_drag_start;
    if (offset > 0) offset = 0;
    if (offset < -700) offset = -700;
    if (offset == lock_drag_offset) {
        return;
    }
    lock_drag_offset = offset;
    LayerEntry *entry = layer_entry_by_surface(lock_drag_surface);
    if (entry != 0) {
        entry->margin[0] = offset;
#ifdef SERVE_ONLY
        scene_dirty = 1;
        compose_full = 1;
#endif
    }
}

static void unlock_now(void) {
    if (keyboard_id == 0) {
        return;
    }
    unsigned time = now_ms();
    keyboard_route(current_client);
    begin(keyboard_id, 3 /* key */);
    put_u32(configure_serial);
    put_u32(time);
    put_u32(57); /* KEY_SPACE */
    put_u32(1);
    finish();
    send_message(clients[current_client].fd, -1);
    begin(keyboard_id, 3);
    put_u32(configure_serial);
    put_u32(time + 1);
    put_u32(57);
    put_u32(0);
    finish();
    send_message(clients[current_client].fd, -1);
}

static void lock_drag_end(void) {
    if (!lock_drag) {
        return;
    }
    lock_drag = 0;
    int unlock = lock_drag_offset < -120;
    LayerEntry *entry = layer_entry_by_surface(lock_drag_surface);
    if (entry != 0) {
        entry->margin[0] = 0;
    }
#ifdef SERVE_ONLY
    scene_dirty = 1;
    compose_full = 1;
#endif
    if (unlock) {
        unlock_now();
    }
}

static void poll_input(void) {
    if (screen == 0) {
        return;
    }
    /* Events go to the application window when one is mapped, otherwise to
       the shell (the client that owns the layer scene). */
    int index = (app_window.used && !app_window.hidden) ? app_window.owner
                                                       : ui_client;
    if (index < 0 || index >= MAX_CLIENTS || !clients[index].connected) {
        return;
    }
    long client = clients[index].fd;
    current_client = index;
    load_client(&clients[index]);
    /* Both fields hold ring positions (0..INPUT_SLOTS-1), as documented in
       userspace/src/window_proto.rs and written by the host viewer. Keeping
       an unbounded local tail made the two sides disagree from the first
       wrap on: every new event was then read from a stale slot, which looked
       like "the mouse stops working" after ~64 moves. */
    unsigned head = (*(volatile unsigned *)(screen + INPUT_HEAD)) % INPUT_SLOTS;
    unsigned tail = (*(volatile unsigned *)(screen + INPUT_TAIL)) % INPUT_SLOTS;
    unsigned pending = (head + INPUT_SLOTS - tail) % INPUT_SLOTS;
    unsigned guard = 0;
    while (pending != 0 && guard++ < INPUT_SLOTS) {
        const unsigned char *entry =
            (const unsigned char *)(screen + INPUT_RING + tail * INPUT_ENTRY);
        unsigned kind = entry[0];
        unsigned value = entry[1];
        unsigned key = entry[2] | ((unsigned)entry[3] << 8);
        unsigned x = entry[4] | ((unsigned)entry[5] << 8);
        unsigned y = entry[6] | ((unsigned)entry[7] << 8);
        if (kind == INPUT_KEY && key == 18 /* KEY_NEW_TAB: Ctrl+T */) {
            /* There is no window manager and no launcher yet, so Ctrl+T is
               the way between the shell and the application window: it shows
               or hides the newest toplevel and launches the terminal when
               none is mapped. The host viewer only forwards printable keys
               and the small control codes, so Ctrl+T is one of the few
               combinations it can actually send. */
            toggle_app_window();
        } else if (kind == INPUT_MOVE) {
            if (lock_drag) {
                lock_drag_move(y);
            } else if (window_drag) {
                int nx = (int)x - window_drag_dx;
                int ny = (int)y - window_drag_dy;
                int content_width = (int)app_window.width -
                                    2 * (int)app_window.geometry_x;
                if (content_width <= 0) content_width = (int)app_window.width;
                if (nx > (int)LOGICAL_WIDTH - 80) nx = (int)LOGICAL_WIDTH - 80;
                if (nx < -content_width + 80) nx = -content_width + 80;
                if (ny < 0) ny = 0;
                if (ny > (int)LOGICAL_HEIGHT - 40) ny = (int)LOGICAL_HEIGHT - 40;
                if (nx != app_window.x || ny != app_window.y) {
                    app_window.x = nx;
                    app_window.y = ny;
#ifdef SERVE_ONLY
                    scene_dirty = 1;
                    compose_full = 1;
#endif
                }
            } else if (value == 1) {
                /* The host pointer left the window. */
                if (pointer_id != 0 && pointer_target != 0) {
                    pointer_send_leave(client, pointer_target);
                    pointer_target = 0;
                }
            } else if (drag_active != 0) {
                pointer_x = x;
                pointer_y = y;
                drag_motion(client, x, y);
            } else {
                pointer_x = x;
                pointer_y = y;
                pointer_route(client, x, y, 1);
            }
        } else if (kind == INPUT_CLICK || kind == INPUT_RELEASE) {
            if (kind == INPUT_CLICK && app_window_chrome(x, y)) {
                /* The compositor's own window chrome claimed the press. */
                chrome_grab = 1;
            } else if (kind == INPUT_RELEASE && lock_drag) {
                lock_drag_end();
                chrome_grab = 0;
            } else if (kind == INPUT_RELEASE && chrome_grab) {
                /* The press never reached the client; swallow the release. */
                chrome_grab = 0;
            } else if (kind == INPUT_CLICK && drag_begin(client, x, y)) {
                /* A layer-surface drag claims the press. */
            } else if (kind == INPUT_CLICK &&
                       (chrome_grab = lock_drag_begin(x, y))) {
                /* The lock screen has no drag surface and Phosh's unlock is a
                   touch swipe, which a mouse cannot produce. A click on it is
                   the closest thing; with require-unlock off a key press
                   unlocks, so a click sends one. */
            } else if (kind == INPUT_RELEASE && window_drag) {
                window_drag = 0;
            } else if (kind == INPUT_RELEASE && drag_active != 0) {
                drag_finish(client);
            } else if (pointer_id != 0) {
                /* A click can arrive without a preceding motion (the viewer
                   only sends moves while hovering). Route it first so the
                   button goes to the surface under the cursor. */
                /* Send the position, not just the enter: GTK positions a
                   click from the last motion/enter it saw, and a release with
                   stale coordinates can land outside the widget that was
                   pressed (the click then only focuses it). */
                pointer_route(client, x, y, 1);
                if (kind == INPUT_CLICK) {
                    pointer_grab = pointer_target;
                }
                unsigned button = (value & 3) == 2 ? 0x112u : 0x110u;
                begin(pointer_id, 3 /* button */);
                put_u32(configure_serial);
                put_u32(now_ms()); /* time */
                put_u32(button);
                put_u32(kind == INPUT_CLICK ? 1u : 0u);
                finish();
                send_message(client, -1);
                pointer_frame(client);
                if (kind == INPUT_RELEASE) {
                    pointer_grab = 0;
                }
            }
        } else if (kind == INPUT_KEY) {
            /* The viewer speaks Unicode/control codes, the client speaks
               evdev keycodes. An unknown value is consumed and dropped so it
               cannot desynchronise the ring. */
            unsigned shift = 0;
            unsigned keycode = keycode_for(key, &shift);
            if (keycode != 0) {
                grab_activate(client, keycode);
                if (keyboard_id != 0) {
                    keyboard_route(client);
                    unsigned time = now_ms();
                    if (shift) {
                        /* A shifted character needs the mask before the key
                           event; GTK translates the keycode through xkb. */
                        keyboard_send_modifiers(client, 1 /* shift */);
                    }
                    begin(keyboard_id, 3 /* key */);
                    put_u32(configure_serial);
                    put_u32(time);
                    put_u32(keycode);
                    put_u32(1); /* pressed */
                    finish();
                    send_message(client, -1);
                    /* A release is mandatory: GTK implements key repeat
                       itself from wl_keyboard.repeat_info, so a key that never
                       goes up keeps activating (the terminal ran Enter four
                       times). */
                    begin(keyboard_id, 3 /* key */);
                    put_u32(configure_serial);
                    put_u32(time + 1);
                    put_u32(keycode);
                    put_u32(0); /* released */
                    finish();
                    send_message(client, -1);
                    if (shift) {
                        keyboard_send_modifiers(client, 0);
                    }
                }
            }
        } else if (kind == INPUT_SCROLL) {
            if (pointer_id != 0) {
                begin(pointer_id, 4 /* axis */);
                put_u32(now_ms()); /* time */
                put_u32(0); /* vertical */
                put_i32((int)value * 10 * 256);
                finish();
                send_message(client, -1);
                pointer_frame(client);
            }
        }
        tail = (tail + 1) % INPUT_SLOTS;
        *(volatile unsigned *)(screen + INPUT_TAIL) = tail;
        pending -= 1;
    }
    save_client(&clients[index]);
}

static int serve(long client) {
    long count;
    do {
        count = receive_message(client);
        /* A partial message must be completed before the multi-client loop
           switches away: `incoming` and `incoming_pending` are shared by all
           clients. Only an idle client (no pending tail) yields on timeout. */
    } while (count == -2 && incoming_pending != 0);
    if (count == -2) {
        /* Frame clock tick. GTK requests wl_surface.frame and waits for the
           done event before running its tick callbacks; completing it only on
           commit stalled every animation that had nothing new to draw (the
           top panel never finished unfolding). Flushing on the idle timeout
           acts as a ~50 Hz vblank. */
        for (unsigned index = 0; index < SURFACE_IDS; index++) {
            if (frame_ids[index] != 0) {
                send_done(client, frame_ids[index], now_ms());
                frame_ids[index] = 0;
            }
        }
        return 0; /* input serviced, no client message */
    }
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
                send_global_version(client, registry_id, 13,
                                    "wp_presentation", 3);
                send_global(client, registry_id, 14,
                            "zwp_relative_pointer_manager_v1");
                send_global(client, registry_id, 15,
                            "zwp_pointer_constraints_v1");
                send_global(client, registry_id, 16,
                            "zwp_text_input_manager_v3");
                send_global_version(client, registry_id, 17,
                                    "zwlr_layer_shell_v1", 4);
                send_global(client, registry_id, 18, "ext_idle_notifier_v1");
                send_global(client, registry_id, 19,
                            "zwlr_output_power_manager_v1");
                send_global_version(client, registry_id, 20,
                                    "zphoc_layer_shell_effects_v1", 3);
                send_global_version(client, registry_id, 21,
                                    "zwlr_foreign_toplevel_manager_v1", 3);
                send_global(client, registry_id, 22,
                            "zwlr_gamma_control_manager_v1");
                send_global_version(client, registry_id, 23,
                                    "zwlr_output_manager_v1", 2);
                send_global_version(client, registry_id, 24,
                                    "phosh_private", 7);
                send_global_version(client, registry_id, 25,
                                    "zwp_linux_dmabuf_v1", 4);
            } else if (object == registry_id && opcode == 0) { /* bind */
                (void)take_u32();
                char interface[64];
                take_string(interface, sizeof(interface));
                unsigned version = take_u32();
                unsigned new_id = take_u32();
                if (text_equal(interface, "wl_compositor")) {
                    compositor_id = new_id;
                } else if (text_equal(interface, "zwp_linux_dmabuf_v1")) {
                    dmabuf_id = new_id;
                    for (unsigned index = 0; index < 8; index++) {
                        if (dmabuf_ids[index] == 0) {
                            dmabuf_ids[index] = new_id;
                            break;
                        }
                    }
                    {
                        static const char bound[] = "gtk-serve: dmabuf bound\n";
                        sys3(64, 1, (long)bound, sizeof(bound) - 1);
                    }
                    /* Version 4 replaces the format/modifier events with
                       dma-buf feedback; older bindings still get the plain
                       linear format list. */
                    if (version < 4) {
                        static const unsigned formats[] = {0 /* ARGB8888 */,
                                                           1 /* XRGB8888 */};
                        for (unsigned index = 0; index < 2; index++) {
                            begin(new_id, 0 /* format */);
                            put_u32(formats[index]);
                            finish();
                            send_message(client, -1);
                            if (version >= 3) {
                                begin(new_id, 1 /* modifier */);
                                put_u32(formats[index]);
                                put_u32(0); /* modifier_hi */
                                put_u32(0); /* modifier_lo: LINEAR */
                                finish();
                                send_message(client, -1);
                            }
                        }
                    }
                } else if (text_equal(interface, "wl_shm")) {
                    shm_id = new_id;
                    shm_add(new_id);
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
                        put_i32((int)OUTPUT_SCALE);
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
                } else if (text_equal(interface,
                                      "zwp_relative_pointer_manager_v1")) {
                    relative_manager_id = new_id;
                } else if (text_equal(interface,
                                      "zwp_pointer_constraints_v1")) {
                    constraints_id = new_id;
                } else if (text_equal(interface,
                                      "zwp_text_input_manager_v3")) {
                    text_input_manager_id = new_id;
                } else if (text_equal(interface, "zwlr_layer_shell_v1")) {
                    layer_shell_id = new_id;
                } else if (text_equal(interface, "wp_presentation")) {
                    presentation_id = new_id;
                    begin(presentation_id, 0 /* clock_id */);
                    put_u32(1); /* CLOCK_MONOTONIC */
                    finish(); send_message(client, -1);
                } else if (text_equal(interface, "ext_idle_notifier_v1")) {
                    idle_notifier_id = new_id;
                } else if (text_equal(interface,
                                      "zwlr_output_power_manager_v1")) {
                    output_power_manager_id = new_id;
                } else if (text_equal(interface,
                                      "zphoc_layer_shell_effects_v1")) {
                    layer_effects_id = new_id;
                } else if (text_equal(interface,
                                      "zwlr_foreign_toplevel_manager_v1")) {
                    foreign_toplevel_manager_id = new_id;
                } else if (text_equal(interface,
                                      "zwlr_gamma_control_manager_v1")) {
                    gamma_manager_id = new_id;
                } else if (text_equal(interface, "zwlr_output_manager_v1")) {
                    output_manager_id = new_id;
                } else if (text_equal(interface, "phosh_private")) {
                    phosh_private_id = new_id;
                }
            } else if (object == text_input_manager_id && opcode == 1) {
                text_input_id = take_u32(); /* get_text_input */
                (void)take_u32(); /* seat */
            } else if (object == text_input_id && opcode == 1) {
                text_input_enabled = 1;
                if (surface_id != 0) {
                    begin(text_input_id, 0 /* enter */);
                    put_u32(surface_id);
                    finish(); send_message(client, -1);
                }
            } else if (object == text_input_id && opcode == 2) {
                if (text_input_enabled && surface_id != 0) {
                    begin(text_input_id, 1 /* leave */);
                    put_u32(surface_id);
                    finish(); send_message(client, -1);
                }
                text_input_enabled = 0;
            } else if (object == text_input_id && opcode == 7) {
                if (text_input_enabled) {
                    text_input_serial += 1;
                    begin(text_input_id, 3 /* commit_string */);
                    put_string("werewolf");
                    finish(); send_message(client, -1);
                    begin(text_input_id, 5 /* done */);
                    put_u32(text_input_serial);
                    finish(); send_message(client, -1);
                }
            } else if (object == text_input_id && opcode == 0) {
                text_input_id = 0;
                text_input_enabled = 0;
            } else if (object == relative_manager_id && opcode == 1) {
                relative_pointer_id = take_u32();
                (void)take_u32(); /* pointer */
                begin(relative_pointer_id, 0 /* relative_motion */);
                put_u32(0); put_u32(0);
                put_i32(3 << 8); put_i32(4 << 8);
                put_i32(3 << 8); put_i32(4 << 8);
                finish(); send_message(client, -1);
            } else if (object == relative_pointer_id && opcode == 0) {
                relative_pointer_id = 0;
            } else if (object == constraints_id && opcode == 1) {
                locked_id = take_u32();
                (void)take_u32(); /* surface */
                (void)take_u32(); /* pointer */
                (void)take_u32(); /* region */
                (void)take_u32(); /* lifetime */
                begin(locked_id, 0 /* locked */);
                finish(); send_message(client, -1);
            } else if (object == locked_id && opcode == 0) {
                locked_id = 0;
            } else if (object == presentation_id && opcode == 1) {
                (void)take_u32(); /* surface */
                feedback_id = take_u32();
                if (output_id != 0) {
                    begin(feedback_id, 0 /* sync_output */);
                    put_u32(output_id);
                    finish(); send_message(client, -1);
                }
                begin(feedback_id, 1 /* presented */);
                put_u32(0); /* tv_sec_hi */
                put_u32(0); /* tv_sec_lo */
                put_u32(0); /* tv_nsec */
                put_u32(16666666); /* refresh: 60 Hz */
                put_u32(0); /* seq_hi */
                put_u32(0); /* seq_lo */
                put_u32(1); /* vsync */
                finish(); send_message(client, -1);
            } else if (object == feedback_id && opcode == 0) {
                feedback_id = 0;
            } else if (object == fractional_manager_id && opcode == 1) {
                fractional_id = take_u32(); /* get_fractional_scale */
                (void)take_u32(); /* surface */
                begin(fractional_id, 0 /* preferred_scale */);
                put_u32(120u * OUTPUT_SCALE);
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
                /* Logical size is the mode divided by wl_output.scale: a
                   1024x768 buffer at scale 2 is a 512x384 desktop. Reporting
                   1024x768 here made Phosh size its panels against a screen
                   twice as large as the one it was actually drawing. */
                put_i32((int)LOGICAL_WIDTH); put_i32((int)LOGICAL_HEIGHT);
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
                int passed_fd = take_incoming_fd();
                if (passed_fd >= 0 && clipboard_owner >= 0 &&
                    clipboard_source_id != 0) {
                    begin(clipboard_source_id, 1 /* wl_data_source.send */);
                    put_string(mime);
                    finish();
                    send_message(clients[clipboard_owner].fd, passed_fd);
                    passed_fd = -1;
                }
                if (passed_fd >= 0) {
                    sys3(57, passed_fd, 0, 0);
                }
            } else if (object == data_offer_id && opcode == 2) {
                data_offer_id = 0;
            } else if (object == compositor_id && opcode == 0) {
                unsigned new_surface = take_u32(); /* create_surface */
                if (new_surface < SURFACE_IDS) {
                    surface_ids[new_surface] = 1;
                }
                if (surface_id == 0) {
                    surface_id = new_surface;
                } else if (popup_surface_id == 0) {
                    popup_surface_id = new_surface;
                } else if (subsurface_surface_id == 0) {
                    subsurface_surface_id = new_surface;
                }
            } else if (object == compositor_id && opcode == 1) {
                /* wl_compositor.create_region(new_id) */
                unsigned id = take_u32();
                region_add(id);
            } else if (region_find(object) != 0 && opcode == 0) {
                /* wl_region.destroy */
                region_forget(object);
            } else if (region_find(object) != 0 && opcode == 1) {
                /* wl_region.add(x, y, width, height). wl_region has destroy=0,
                   add=1, subtract=2; mixing them up dropped every rectangle
                   and marked interactive surfaces as input-transparent. */
                RegionSlot *region = region_find(object);
                int x = (int)take_u32();
                int y = (int)take_u32();
                int width = (int)take_u32();
                int height = (int)take_u32();
                if (region->rects == 0) {
                    region->x0 = x;
                    region->y0 = y;
                    region->x1 = x + width;
                    region->y1 = y + height;
                } else {
                    if (x < region->x0) region->x0 = x;
                    if (y < region->y0) region->y0 = y;
                    if (x + width > region->x1) region->x1 = x + width;
                    if (y + height > region->y1) region->y1 = y + height;
                }
                region->rects += 1;
            } else if (region_find(object) != 0 && opcode == 2) {
                /* wl_region.subtract: keep the outer box (conservative). */
                (void)take_u32();
                (void)take_u32();
                (void)take_u32();
                (void)take_u32();
            } else if (object == shell_id && opcode == 1) {
                positioner_id = take_u32(); /* create_positioner */
            } else if (object == shell_id && opcode == 2) {
                /* xdg_wm_base.get_xdg_surface(new_id, surface).
                   The role is not known yet: the next request on this
                   xdg_surface is either get_toplevel or get_popup. Surface
                   creation order is NOT a role hint — GTK4 creates cursor and
                   auxiliary surfaces before the toplevel, which used to put
                   the real toplevel in the popup slot and suppress its
                   configure. */
                unsigned new_xdg_surface = take_u32();
                unsigned surface = take_u32();
                xdg_surface_id = new_xdg_surface;
                surface_id = surface;
                if (subsurface_surface_id == surface) {
                    subsurface_surface_id = 0;
                }
                if (popup_surface_id == surface) {
                    popup_surface_id = 0;
                }
            } else if (object == xdg_surface_id && opcode == 1) {
                /* xdg_surface.get_toplevel(new_id): send configure. */
                toplevel_id = take_u32();
                toplevel_surface_id = surface_id;
                if (current_client != ui_client) {
                    app_surface_id = surface_id;
                }
                begin(toplevel_id, 0 /* xdg_toplevel.configure */);
                put_i32(APP_WIN_WIDTH);
                put_i32(APP_WIN_HEIGHT);
                put_u32(0);    /* states array: empty */
                finish();
                send_message(client, -1);
                begin(xdg_surface_id, 0 /* xdg_surface.configure */);
                put_u32(configure_serial);
                configure_serial += 1;
                finish();
                send_message(client, -1);
            } else if (object == xdg_surface_id && opcode == 2) {
                /* xdg_surface.get_popup(new_id, parent, positioner): the
                   surface bound to this xdg_surface is a popup, not the
                   toplevel. Move it to the popup slot and restore the
                   toplevel surface for enter/focus events. */
                popup_xdg_surface_id = xdg_surface_id;
                popup_surface_id = surface_id;
                xdg_surface_id = 0;
                surface_id = toplevel_surface_id;
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
            } else if (object == xdg_surface_id && opcode == 3) {
                /* set_window_geometry(x, y, width, height): where the window
                   content (without its decoration margins) starts. */
                window_geometry_x = take_u32();
                window_geometry_y = take_u32();
                (void)take_u32();
                (void)take_u32();
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
                /* Send NO_KEYMAP: a protocol-valid empty keymap. The old
                   "WEREWOLF_KEYMAP" placeholder made xkbcommon fail to parse
                   the compositor keymap on every GTK client. */
                unsigned length = 0;
                long keymap = sys3(279 /* memfd_create */, (long)"keymap", 0, 0);
                if (keymap >= 0 && sys3(46 /* ftruncate */, keymap, length, 0) >= 0 &&
                    sys3(62 /* lseek */, keymap, 0, 0) == 0) {
                    begin(keyboard_id, 0 /* keymap */);
                    put_u32(0); /* NO_KEYMAP */
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
            } else if (is_shm(object) && opcode == 0) {
                unsigned id = take_u32(); /* create_pool */
                (void)take_i32();
                pool_id = id;
                int pool_descriptor = take_incoming_fd();
                if (pool_descriptor >= 0) {
                    PoolSlot *slot = pool_add(id, pool_descriptor);
                    pool_fd = slot->fd;
                }
            } else if (pool_find(object) != 0 && opcode == 0) {
                /* wl_shm_pool.create_buffer(new_id, offset, w, h, stride,
                   format). Any live pool may create a buffer, not just the
                   most recently created one. */
                unsigned id = take_u32();
                unsigned offset = take_u32();
                unsigned width = take_u32();
                unsigned height = take_u32();
                unsigned stride = take_u32();
                unsigned format = take_u32();
                PoolSlot *pool = pool_find(object);
                BufferSlot *slot = buffer_add(id);
                                if (slot != 0) {
                    slot->pool_fd = pool->fd;
                    slot->offset = offset;
                    slot->width = width;
                    slot->height = height;
                    slot->stride = stride;
                    slot->format = format;
                }
                buffer_id = id;
                buffer_offset = offset;
                buffer_width = (int)width;
                buffer_height = (int)height;
                buffer_stride = stride;
                buffer_format = format;
                pool_fd = pool->fd;
            } else if (pool_find(object) != 0 && opcode == 1) {
                /* wl_shm_pool.destroy: keep the descriptor open, buffers
                   created from this pool may still be committed. */
                PoolSlot *pool = pool_find(object);
                pool->id = 0;
            } else if (dmabuf_is_feedback(object)) {
                /* zwp_linux_dmabuf_feedback_v1.destroy */
                for (unsigned index = 0; index < 16; index++) {
                    if (dmabuf_feedbacks[index] == object) {
                        dmabuf_feedbacks[index] = 0;
                    }
                }
            } else if (dmabuf_is_binding(object) && opcode == 1) {
                /* zwp_linux_dmabuf_v1.create_params(new_id) */
                dmabuf_params_add(take_u32());
            } else if (dmabuf_is_binding(object) &&
                       (opcode == 2 || opcode == 3)) {
                /* get_default_feedback(new_id) or
                   get_surface_feedback(new_id, surface) */
                unsigned id = take_u32();
                if (opcode == 3) {
                    (void)take_u32(); /* wl_surface */
                }
                for (unsigned index = 0; index < 16; index++) {
                    if (dmabuf_feedbacks[index] == 0) {
                        dmabuf_feedbacks[index] = id;
                        break;
                    }
                }
                send_dmabuf_feedback(client, id);
            } else if (dmabuf_is_binding(object) && opcode == 0) {
                /* zwp_linux_dmabuf_v1.destroy */
                for (unsigned index = 0; index < 8; index++) {
                    if (dmabuf_ids[index] == object) {
                        dmabuf_ids[index] = 0;
                    }
                }
            } else if (dmabuf_params_find(object) != 0) {
                DmabufParams *params = dmabuf_params_find(object);
                if (opcode == 0) { /* destroy */
                    if (params->fd >= 0) {
                        sys3(57 /* close */, params->fd, 0, 0);
                    }
                    params->id = 0;
                    params->fd = -1;
                    params->has_plane = 0;
                } else if (opcode == 1) { /* add(fd, plane, offset, stride, mod) */
                    (void)take_u32();     /* plane index, always 0 here */
                    int plane_descriptor = take_incoming_fd();
                    if (plane_descriptor >= 0) {
                        if (params->fd >= 0) {
                            sys3(57 /* close */, params->fd, 0, 0);
                        }
                        params->fd = plane_descriptor;
                        params->has_plane = 1;
                    }
                    params->offset = take_u32();
                    params->stride = take_u32();
                    (void)take_u32(); /* modifier_hi */
                    (void)take_u32(); /* modifier_lo */
                } else if (opcode == 2 || opcode == 3) {
                    /* create(width, height, format, flags, buffer) or
                       create_immed(buffer, width, height, format, flags) */
                    unsigned width, height, format, id;
                    if (opcode == 2) {
                        width = take_u32();
                        height = take_u32();
                        format = take_u32();
                        (void)take_u32(); /* flags */
                        id = take_u32();
                    } else {
                        id = take_u32();
                        width = take_u32();
                        height = take_u32();
                        format = take_u32();
                        (void)take_u32(); /* flags */
                    }
                    if (params->has_plane && params->fd >= 0) {
                        BufferSlot *slot = buffer_add(id);
                        if (slot != 0) {
                            slot->pool_fd = params->fd;
                            slot->offset = params->offset;
                            slot->width = width;
                            slot->height = height;
                            slot->stride = params->stride != 0
                                               ? params->stride
                                               : width * 4;
                            slot->format = format;
                            slot->owned = 1;
                            params->fd = -1; /* ownership moved */
                            params->has_plane = 0;
                            log_dmabuf_buffer(width, height, slot->stride, format);
                        }
                    }
                    params->id = 0;
                }
            } else if (buffer_find(object) != 0 && opcode == 0) {
                /* wl_buffer.destroy. Toolkits create one buffer per frame
                   and destroy the previous one, so without this the table
                   filled up and commits started resolving to stale slots. */
                buffer_forget(object);
            } else if (object == layer_shell_id && opcode == 0) {
                /* get_layer_surface(new_id, surface, output, layer, ns) */
                unsigned new_layer_surface = take_u32();
                unsigned layer_surface_for = take_u32();
                layer_surface_id = new_layer_surface;
                layer_surface_surface = layer_surface_for;
                LayerSurface *slot = layer_surface_add(new_layer_surface, layer_surface_for);
                                (void)take_u32(); /* output, may be null */
                slot->layer = take_u32();
                char space[64];
                take_string(space, sizeof(space));
                /* Configure the surface as soon as it exists. GTK sizes its
                   window from this event and does not commit a buffer before
                   it arrives; Phosh only calls `set_size` for surfaces it
                   resizes, so a window that fills the output (the home /
                   overview surface) was never configured and never drew. */
                begin(new_layer_surface, 0 /* configure */);
                put_u32(configure_serial);
                configure_serial += 1;
                put_u32(LOGICAL_WIDTH);
                put_u32(LOGICAL_HEIGHT);
                finish();
                send_message(client, -1);
                slot->configured = 1;
            } else if (layer_surface_by_id(object) != 0 && opcode == 0) {
                LayerSurface *slot = layer_surface_by_id(object);
                slot->width = take_u32();
                slot->height = take_u32();
                slot->pending = 1;
                layer_width = slot->width;
                layer_height = slot->height;
            } else if (layer_surface_by_id(object) != 0 && opcode == 1) {
                LayerSurface *slot = layer_surface_by_id(object);
                slot->anchor = take_u32();
                layer_anchor = slot->anchor;
            } else if (layer_surface_by_id(object) != 0 && opcode == 2) {
                (void)take_u32(); /* exclusive zone */
            } else if (layer_surface_by_id(object) != 0 && opcode == 3) {
                LayerSurface *slot = layer_surface_by_id(object);
                slot->margin[0] = (int)take_u32();
                slot->margin[1] = (int)take_u32();
                slot->margin[2] = (int)take_u32();
                slot->margin[3] = (int)take_u32();
                for (unsigned index = 0; index < 4; index++) {
                    layer_margin[index] = slot->margin[index];
                }
            } else if (layer_surface_by_id(object) != 0 && opcode == 4) {
                LayerSurface *slot = layer_surface_by_id(object);
                slot->keyboard_interactivity = take_u32();
            } else if (layer_surface_by_id(object) != 0 && opcode == 5) {
                (void)take_u32(); /* get_popup */
            } else if (layer_surface_by_id(object) != 0 && opcode == 6) {
                LayerSurface *slot = layer_surface_by_id(object);
                (void)take_u32(); /* ack_configure serial */
                slot->acked = 1;
                layer_acked = 1;
            } else if (layer_surface_by_id(object) != 0 && opcode == 7) {
                LayerSurface *slot = layer_surface_by_id(object);
                if (slot != 0) {
                    layer_forget_surface(slot->surface);
                    if (pointer_target == slot->surface) {
                        pointer_target = 0;
                    }
                    if (keyboard_target == slot->surface) {
                        keyboard_target = 0;
                    }
                    if (focus_surface == slot->surface) {
                        focus_surface = 0;
                    }
                    slot->used = 0;
                }
                layer_surface_id = 0;
                layer_surface_surface = 0;
                layer_configured = 0;
            } else if (layer_surface_by_id(object) != 0 && opcode == 8) {
                LayerSurface *slot = layer_surface_by_id(object);
                slot->layer = take_u32();
                layer_layer = slot->layer;
            } else if (object == phosh_private_id && opcode == 0) {
                /* rotate_display(wl_surface, degree): unused */
                (void)take_u32();
                (void)take_u32();
            } else if (object == phosh_private_id && opcode == 1) {
                /* get_xdg_switcher(new_id) */
                xdg_switcher_id = take_u32();
            } else if (object == phosh_private_id && opcode == 2) {
                /* get_thumbnail(new_id, toplevel, max_w, max_h) */
                (void)take_u32();
                (void)take_u32();
                (void)take_u32();
                (void)take_u32();
            } else if (object == phosh_private_id && opcode == 3) {
                /* get_keyboard_event(new_id) */
                keyboard_event_id = take_u32();
            } else if (object == phosh_private_id && opcode == 4) {
                /* get_startup_tracker(new_id) */
                startup_tracker_id = take_u32();
            } else if (object == phosh_private_id && opcode == 5) {
                (void)take_u32(); /* set_shell_state(state) */
            } else if (object == startup_tracker_id && opcode == 0) {
                startup_tracker_id = 0;
            } else if (object == keyboard_event_id && opcode == 0) {
                /* grab_accelerator_request(accelerator) */
                char accelerator[128];
                take_string(accelerator, sizeof(accelerator));
                GrabSlot *slot = grab_add(accelerator);
                if (slot != 0) {
                    grab_success(client, slot);
                }
            } else if (object == keyboard_event_id && opcode == 1) {
                unsigned action = take_u32();
                for (unsigned index = 0; index < MAX_GRABS; index++) {
                    if (grabs[index].used && grabs[index].action == action) {
                        grabs[index].used = 0;
                    }
                }
                begin(keyboard_event_id, 4 /* ungrab_success_event */);
                put_u32(action);
                finish();
                send_message(client, -1);
            } else if (object == keyboard_event_id && opcode == 2) {
                keyboard_event_id = 0;
            } else if (object == xdg_switcher_id && opcode == 0) {
                /* list_xdg_surfaces: nothing to report */
            } else if (object == xdg_switcher_id && opcode == 1) {
                xdg_switcher_id = 0;
            } else if (object == xdg_switcher_id && (opcode == 2 || opcode == 3)) {
                char app_id[128];
                char title[128];
                take_string(app_id, sizeof(app_id));
                take_string(title, sizeof(title));
            } else if (object == idle_notifier_id && opcode == 1) {
                /* ext_idle_notifier_v1.get_idle_notification */
                idle_notification_id = take_u32();
                (void)take_u32(); /* seat */
                (void)take_u32(); /* timeout ms */
                begin(idle_notification_id, 1 /* resumed */);
                finish(); send_message(client, -1);
            } else if (object == idle_notifier_id && opcode == 2) {
                /* get_input_idle_notification */
                idle_notification_id = take_u32();
                (void)take_u32();
                (void)take_u32();
                begin(idle_notification_id, 1 /* resumed */);
                finish(); send_message(client, -1);
            } else if (object == idle_notifier_id && opcode == 0) {
                idle_notifier_id = 0;
            } else if (object == idle_notification_id && opcode == 0) {
                idle_notification_id = 0;
            } else if (object == output_power_manager_id && opcode == 0) {
                /* zwlr_output_power_manager_v1.get_output_power */
                output_power_id = take_u32();
                (void)take_u32(); /* output */
                begin(output_power_id, 0 /* mode */);
                put_u32(1); /* on */
                finish(); send_message(client, -1);
            } else if (object == output_power_manager_id && opcode == 1) {
                output_power_manager_id = 0;
            } else if (object == output_power_id && opcode == 0) {
                unsigned mode = take_u32();
                begin(output_power_id, 0 /* mode */);
                put_u32(mode);
                finish(); send_message(client, -1);
            } else if (object == output_power_id && opcode == 1) {
                output_power_id = 0;
            } else if (object == layer_effects_id && opcode == 0) {
                /* zphoc_layer_shell_effects_v1.get_draggable_layer_surface */
                unsigned new_drag = take_u32();
                unsigned drag_for = take_u32();
                draggable_id = new_drag;
                drag_slot_add(new_drag, drag_for);
            } else if (object == layer_effects_id && opcode == 1) {
                layer_effects_id = 0;
            } else if (object == layer_effects_id && opcode == 2) {
                /* get_alpha_layer_surface(new_id, layer_surface) */
                alpha_id = take_u32();
                (void)alpha_slot_add(alpha_id, take_u32());
            } else if (object == layer_effects_id && opcode == 3) {
                /* get_stacked_layer_surface(new_id, layer_surface) */
                unsigned new_stacked = take_u32();
                unsigned for_surface = take_u32();
                stacked_id = new_stacked;
                stacked_add(new_stacked, for_surface);
            } else if (drag_slot_by_id(object) != 0 && opcode <= 5) {
                DragSlot *slot = drag_slot_by_id(object);
                if (opcode == 0) {
                    /* set_margins(folded, unfolded): the surface starts
                       folded, so apply that margin and tell the client. */
                    slot->folded = (int)take_u32();
                    slot->unfolded = (int)take_u32();
                    /* The margins are updated whenever the surface size
                       changes; while unfolded a new folded margin must not
                       move the surface. */
                    if (!slot->dragging) {
                        slot->current =
                            slot->state ? slot->unfolded : slot->folded;
                    }
                    slot->reported = 0;
                    drag_apply(slot->layer_surface);
                    drag_recompose();
                } else if (opcode == 1) {
                    slot->threshold = take_u32();
                    if (slot->threshold > 256) {
                        slot->threshold = 256;
                    }
                } else if (opcode == 2) {
                    slot->exclusive = take_u32();
                } else if (opcode == 3) {
                    slot->drag_mode = take_u32();
                } else if (opcode == 4) {
                    slot->drag_handle = take_u32();
                } else {
                    /* set_state(folded|unfolded): Phosh asks the compositor to
                       move the surface itself (the Super overview action, the
                       home bar, the quick-settings sheet). The client learns
                       the outcome only from `dragged` + `drag_end`, which
                       update its drag-surface state; moving the margin without
                       answering left Phosh stuck in the folded state. */
                    unsigned state = take_u32();
                    slot->state = state ? 1 : 0;
                    slot->current = slot->state ? slot->unfolded : slot->folded;
                    drag_apply(slot->layer_surface);
                    drag_recompose();
                    drag_notify(client, slot, 0 /* dragged */,
                                (unsigned)slot->current);
                    drag_notify(client, slot, 1 /* drag_end */,
                                (unsigned)slot->state);
                }
            } else if (drag_slot_by_id(object) != 0 && opcode == 6) {
                DragSlot *slot = drag_slot_by_id(object);
                slot->used = 0;
                slot->id = 0;
                draggable_id = 0;
            } else if (alpha_slot_by_id(object) != 0 && opcode == 0) {
                /* set_alpha(wl_fixed): 1.0 is opaque. */
                AlphaSlot *slot = alpha_slot_by_id(object);
                unsigned fixed = take_u32();
                slot->alpha = (fixed * 255u + 127u) / 256u;
                                if (slot->alpha > 255) {
                    slot->alpha = 255;
                }
                LayerSurface *surface = layer_surface_by_id(slot->layer_surface);
                if (surface != 0) {
                    surface->alpha = slot->alpha;
                    surface->alpha_set = 1;
                    /* Phosh animates the quick-settings backdrop and the home
                       background by changing the alpha *after* the surface
                       committed. The stored pixels keep the alpha they were
                       copied with, so the entry has to be updated and the
                       scene recomposed; otherwise a transparent (alpha 0)
                       backdrop stayed opaque and covered the screen. */
                    LayerEntry *entry = layer_entry_by_surface(surface->surface);
                    if (entry != 0) {
                        entry->alpha = slot->alpha;
                        entry->alpha_set = 1;
#ifdef SERVE_ONLY
                        scene_dirty = 1;
                        compose_full = 1;
#endif
                    }
                }
            } else if (alpha_slot_by_id(object) != 0 && opcode == 1) {
                AlphaSlot *slot = alpha_slot_by_id(object);
                slot->alpha = 255;
                slot->used = 0;
                slot->layer_surface = 0;
                alpha_id = 0;
            } else if (stacked_find(object) != 0 && opcode <= 1) {
                /* zphoc_stacked_layer_surface_v1.stack_below / stack_above */
                StackedSlot *stacked = stacked_find(object);
                unsigned sibling = take_u32();
                LayerSurface *target = layer_surface_by_id(stacked->layer_surface);
                if (target != 0) {
                    if (opcode == 0) {
                        target->below = sibling;
                    } else {
                        target->above = sibling;
                    }
                    LayerEntry *entry = layer_entry_by_surface(target->surface);
                    if (entry != 0) {
                        if (opcode == 0) {
                            entry->below = sibling;
                        } else {
                            entry->above = sibling;
                        }
                    }
                }
            } else if (stacked_find(object) != 0 && opcode == 2) {
                StackedSlot *stacked = stacked_find(object);
                stacked->id = 0;
                stacked_id = 0;
            } else if (foreign_handle != 0 && object == foreign_handle) {
                /* zwlr_foreign_toplevel_handle_v1 requests. */
                if (opcode == 1) {
                    close_app_window();
                } else if (opcode == 7) {
                    foreign_handle = 0;
                }
            } else if (object == foreign_toplevel_manager_id && opcode == 0) {
                foreign_toplevel_manager_id = 0;
            } else if (object == gamma_manager_id && opcode == 0) {
                /* get_gamma_control(new_id, output) */
                gamma_control_id = take_u32();
                (void)take_u32();
                begin(gamma_control_id, 0 /* gamma_size */);
                put_u32(256);
                finish(); send_message(client, -1);
            } else if (object == gamma_manager_id && opcode == 1) {
                gamma_manager_id = 0;
            } else if (object == gamma_control_id && opcode == 0) {
                /* set_gamma(fd): consume and close the passed descriptor */
                int gamma_descriptor = take_incoming_fd();
                if (gamma_descriptor >= 0) {
                    sys3(57, gamma_descriptor, 0, 0);
                }
            } else if (object == gamma_control_id && opcode == 1) {
                gamma_control_id = 0;
            } else if (object == output_manager_id && opcode == 0) {
                /* create_configuration(new_id, serial) */
                output_config_id = take_u32();
                (void)take_u32();
            } else if (object == output_manager_id && opcode == 1) {
                output_manager_id = 0;
            } else if (object == output_config_id && opcode == 0) {
                /* enable_head(new_id, head) */
                (void)take_u32();
                (void)take_u32();
            } else if (object == output_config_id && opcode == 1) {
                (void)take_u32(); /* disable_head */
            } else if (object == output_config_id && (opcode == 2 || opcode == 3)) {
                /* apply / test: report success and finish the config */
                begin(output_config_id, 0 /* succeeded */);
                finish(); send_message(client, -1);
                if (opcode == 2) output_config_id = 0;
            } else if (object == output_config_id && opcode == 4) {
                output_config_id = 0;
            } else if (is_surface(object) && opcode == 0) {
                /* wl_surface.destroy: drop focus with leave events. */
                surface_ids[object] = 0;
                if (keyboard_id != 0) {
                    begin(keyboard_id, 2 /* leave */);
                    put_u32(configure_serial);
                    put_u32(object);
                    finish();
                    send_message(client, -1);
                }
                if (pointer_id != 0) {
                    begin(pointer_id, 1 /* leave */);
                    put_u32(configure_serial);
                    put_u32(object);
                    finish();
                    send_message(client, -1);
                }
                layer_forget_surface(object);
                if (pointer_target == object) {
                    pointer_target = 0;
                }
                if (keyboard_target == object) {
                    keyboard_target = 0;
                }
                focused = 0;
                if (surface_id == object) {
                    surface_id = 0;
                }
                if (focus_surface == object) {
                    focus_surface = 0;
                }
            } else if (object == toplevel_id &&
                       (opcode == 2 || opcode == 3)) {
                /* xdg_toplevel.set_title / set_app_id */
                char value[96];
                take_string(value, sizeof(value));
                if (current_client != ui_client) {
                    char *target = opcode == 2 ? toplevel_title
                                               : toplevel_app_id;
                    unsigned index = 0;
                    while (value[index] != 0 && index + 1 < 96) {
                        target[index] = value[index];
                        index += 1;
                    }
                    target[index] = 0;
                }
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
            } else if (object == popup_surface_id && opcode == 0) {
                popup_surface_id = 0;
                popup_xdg_surface_id = 0;
                popup_id = 0;
            } else if (is_surface(object) && opcode == 1) {
                /* wl_surface.attach(buffer, x, y) */
                unsigned buffer = take_u32();
                (void)take_i32();
                attached_buffer = buffer;
                if (object < SURFACE_IDS) {
                    attached_buffers[object] = buffer;
                    if (buffer != 0) {
                        attached_fresh[object] = 1;
                    }
                }
            } else if (is_surface(object) && (opcode == 2 || opcode == 9)) {
                /* wl_surface.damage / damage_buffer: accumulate the union of
                   the rectangles this commit changed. */
                int dx = (int)take_u32();
                int dy = (int)take_u32();
                int dw = (int)take_u32();
                int dh = (int)take_u32();
                if (object < SURFACE_IDS && dw > 0 && dh > 0) {
                    if (damage_valid[object]) {
                        int *box = damage_box[object];
                        int x0 = dx < box[0] ? dx : box[0];
                        int y0 = dy < box[1] ? dy : box[1];
                        int x1 = dx + dw > box[0] + box[2] ? dx + dw
                                                          : box[0] + box[2];
                        int y1 = dy + dh > box[1] + box[3] ? dy + dh
                                                          : box[1] + box[3];
                        box[0] = x0;
                        box[1] = y0;
                        box[2] = x1 - x0;
                        box[3] = y1 - y0;
                    } else {
                        damage_box[object][0] = dx;
                        damage_box[object][1] = dy;
                        damage_box[object][2] = dw;
                        damage_box[object][3] = dh;
                        damage_valid[object] = 1;
                    }
                }
            } else if (is_surface(object) && opcode == 5) {
                /* wl_surface.set_input_region(region): NULL means the whole
                   surface, an empty region means "no input here". */
                unsigned region_id = take_u32();
                if (object < SURFACE_IDS) {
                    if (region_id == 0) {
                        surface_input_set[object] = 0;
                        surface_input_empty[object] = 0;
                    } else {
                        RegionSlot *region = region_find(region_id);
                        surface_input_set[object] = 1;
                        surface_input_empty[object] =
                            (region == 0 || region->rects == 0) ? 1 : 0;
                        if (region != 0 && region->rects != 0) {
                            surface_input_box[object][0] = region->x0;
                            surface_input_box[object][1] = region->y0;
                            surface_input_box[object][2] = region->x1;
                            surface_input_box[object][3] = region->y1;
                        }
                    }
                }
            } else if (is_surface(object) && opcode == 3) {
                /* wl_surface.frame: remember the callback and complete it
                   when this surface is next committed. */
                unsigned callback = take_u32();
                if (object < SURFACE_IDS) {
                    if (frame_ids[object] != 0) {
                        send_done(client, frame_ids[object], now_ms());
                    }
                    frame_ids[object] = callback;
                } else {
                    send_done(client, callback, now_ms());
                }
            } else if (is_surface(object) && opcode == 6) {
                /* wl_surface.commit: report the output, then verify pixels. */
                if (object == subsurface_surface_id) {
                    subsurface_ok = 1;
                }
                if (object == popup_surface_id) {
                    popup_committed = 1;
                    if (popup_grab && popup_id != 0) {
                        begin(popup_id, 1 /* xdg_popup.popup_done */);
                        finish(); send_message(client, -1);
                        popup_grab = 0;
                    }
                }
                LayerSurface *layer_slot = layer_surface_by_surface(object);
                if (layer_slot != 0 && layer_slot->pending) {
                    /* A layer surface asking for size 0 (or -1) leaves the
                       axis to the compositor; answer with the logical output
                       size instead of 0, which GTK rejects. */
                    unsigned configure_width =
                        (layer_slot->width == 0 || layer_slot->width == 0xFFFFFFFFu)
                            ? LOGICAL_WIDTH : layer_slot->width;
                    unsigned configure_height =
                        (layer_slot->height == 0 || layer_slot->height == 0xFFFFFFFFu)
                            ? LOGICAL_HEIGHT : layer_slot->height;
                    begin(layer_slot->layer_surface, 0 /* configure */);
                    put_u32(configure_serial);
                    configure_serial += 1;
                    put_u32(configure_width);
                    put_u32(configure_height);
                    finish();
                    send_message(client, -1);
                                        layer_slot->configured = 1;
                    layer_slot->pending = 0;
                }
                if (object < SURFACE_IDS && !surface_entered[object] &&
                    output_id != 0) {
                    surface_entered[object] = 1;
                    begin(object, 0 /* wl_surface.enter */);
                    put_u32(output_id);
                    finish();
                    send_message(client, -1);
                }
                /* A commit uses the buffer attached to *this* surface. The
                   globals used by the blit are refreshed from the buffer
                   table so interleaved surfaces cannot steal each other's
                   pool or release. */
                unsigned committed_buffer = 0;
                int committed_fresh = 0;
                if (object < SURFACE_IDS) {
                    committed_buffer = attached_buffers[object];
                    committed_fresh = attached_fresh[object];
                    attached_fresh[object] = 0;
                }
                BufferSlot *committed_slot = buffer_find(committed_buffer);
                if (layer_slot != 0) {
                                    }
                if (committed_slot != 0) {
                    buffer_id = committed_slot->id;
                    pool_fd = committed_slot->pool_fd;
                    buffer_offset = committed_slot->offset;
                    buffer_width = (int)committed_slot->width;
                    buffer_height = (int)committed_slot->height;
                    buffer_stride = committed_slot->stride;
                    buffer_format = committed_slot->format;
                }
                if (committed_slot != 0 && pool_fd >= 0 && buffer_id != 0) {
                    commits += 1;
#ifdef SERVE_ONLY
                    if (!frame_reported) {
                        frame_reported = 1;
                        static const char frame_msg[] =
                            "gtk-serve: shm frame committed\n";
                        sys3(64, 1, (long)frame_msg, sizeof(frame_msg) - 1);
                    }
                    /* A layer surface is composed by layer_compose(), which
                       draws the background and every layer in order. Blitting
                       its buffer straight to the sink as well published two
                       different frames per commit (the raw surface first, the
                       full composite second), which showed up as the
                       lockscreen flickering over the wallpaper. Only the
                       single-window path needs the direct blit. */
                    /* Only application clients own the overlay window. The
                       shell's own toplevels (Phosh creates a few while it
                       starts) are not part of the layer scene either, and
                       capturing one made it cover the desktop and made
                       Ctrl+T hide a phantom window instead of launching. */
                    if (layer_slot == 0 && current_client != ui_client &&
                        object == app_surface_id &&
                        buffer_width > 0 && buffer_height > 0 &&
                        buffer_stride >= (unsigned)buffer_width * 4) {
                        /* Keep the newest toplevel as the app window and
                           draw it over the scene in layer_compose(). The
                           buffer is copied while the pool is alive, for the
                           same reason layers are copied. */
                        unsigned bytes = buffer_stride *
                                         (unsigned)buffer_height;
                        if (app_window.copy_bytes < bytes) {
                            if (app_window.copy != 0) {
                                sys3(215, (long)app_window.copy,
                                     app_window.copy_bytes, 0);
                            }
                            long memory = sys6(222, 0, bytes, 3,
                                               0x22 /* PRIVATE|ANON */, -1, 0);
                            if (memory > 0) {
                                app_window.copy = (unsigned char *)memory;
                                app_window.copy_bytes = bytes;
                            } else {
                                app_window.copy = 0;
                                app_window.copy_bytes = 0;
                            }
                        }
                        long mapped = 0;
                        if (app_window.copy != 0) {
                            mapped = sys6(222, 0, buffer_offset + bytes,
                                          1, 1, pool_fd, 0);
                        }
                        if (mapped > 0) {
                            const unsigned char *source =
                                (const unsigned char *)(mapped + buffer_offset);
                            /* Same as a layer: store in sink order and only
                               the damaged part, so a keystroke does not
                               re-swizzle a megapixel window. */
                            int dx0, dy0, dx1, dy1;
                            take_damage((int)object, (int)buffer_width,
                                        (int)buffer_height,
                                        &dx0, &dy0, &dx1, &dy1);
                            if (dx0 < 0) dx0 = 0;
                            if (dy0 < 0) dy0 = 0;
                            if (dx1 > (int)buffer_width) dx1 = (int)buffer_width;
                            if (dy1 > (int)buffer_height) dy1 = (int)buffer_height;
                            /* Raw copy, and only when the pixels differ: a
                               window redraw that changed nothing must not
                               cost a compose. */
                            int app_changed = 0;
                            for (int row = dy0; row < dy1 && !app_changed;
                                 row++) {
                                const unsigned char *source_row =
                                    source + (unsigned)row * buffer_stride;
                                const unsigned char *stored_row =
                                    app_window.copy + (unsigned)row * buffer_stride;
                                int column = dx0;
                                while (column + 2 <= dx1) {
                                    unsigned long long a;
                                    unsigned long long b;
                                    __builtin_memcpy(&a, source_row + (unsigned)column * 4, 8);
                                    __builtin_memcpy(&b, stored_row + (unsigned)column * 4, 8);
                                    if (a != b) {
                                        app_changed = 1;
                                        break;
                                    }
                                    column += 2;
                                }
                                if (!app_changed && column < dx1) {
                                    unsigned a;
                                    unsigned b;
                                    __builtin_memcpy(&a, source_row + (unsigned)column * 4, 4);
                                    __builtin_memcpy(&b, stored_row + (unsigned)column * 4, 4);
                                    if (a != b) {
                                        app_changed = 1;
                                    }
                                }
                            }
                            if (app_changed) {
                                for (int row = dy0; row < dy1; row++) {
                                    const unsigned char *source_row =
                                        source + (unsigned)row * buffer_stride;
                                    unsigned char *destination_row =
                                        app_window.copy + (unsigned)row * buffer_stride;
                                    int column = dx0;
                                    while (column + 2 <= dx1) {
                                        unsigned long long value;
                                        __builtin_memcpy(&value, source_row + (unsigned)column * 4, 8);
                                        __builtin_memcpy(destination_row + (unsigned)column * 4,
                                                         &value, 8);
                                        column += 2;
                                    }
                                    if (column < dx1) {
                                        unsigned value;
                                        __builtin_memcpy(&value, source_row + (unsigned)column * 4, 4);
                                        __builtin_memcpy(destination_row + (unsigned)column * 4,
                                                         &value, 4);
                                    }
                                }
                                __asm__ volatile("dsb sy" ::: "memory");
                            }
                            sys3(215, mapped, buffer_offset + bytes, 0);
                            if (!app_window.used) {
                                static const char frame[] =
                                    "gtk-serve: app frame\n";
                                sys3(64, 1, (long)frame, sizeof(frame) - 1);
                                foreign_toplevel_announce();
                            }
                            app_window.used = 1;
                            app_window.owner = current_client;
                            app_window.surface = object;
                            app_window.geometry_x = window_geometry_x;
                            app_window.geometry_y = window_geometry_y;
                            if (app_window.toplevel == 0) {
                                app_window.toplevel = toplevel_id;
                            }
                            if (app_window.x == 0 && app_window.y == 0) {
                                int content_w = (int)buffer_width -
                                                2 * (int)window_geometry_x;
                                int content_h = (int)buffer_height -
                                                2 * (int)window_geometry_y;
                                if (content_w <= 0) content_w = (int)buffer_width;
                                if (content_h <= 0) content_h = (int)buffer_height;
                                app_window.x =
                                    ((int)LOGICAL_WIDTH - content_w) / 2;
                                app_window.y =
                                    ((int)LOGICAL_HEIGHT - content_h) / 2;
                                if (app_window.y < 40) app_window.y = 40;
                            }
                            app_window.width = (unsigned)buffer_width;
                            app_window.height = (unsigned)buffer_height;
                            app_window.stride = buffer_stride;
                            app_window.format = buffer_format;
                            app_window.copy_ready = 1;
                            if (app_changed) {
                                int origin_x = app_window.x -
                                               (int)app_window.geometry_x;
                                int origin_y = app_window.y -
                                               (int)app_window.geometry_y;
                                compose_damage(origin_x + dx0, origin_y + dy0,
                                               origin_x + dx1, origin_y + dy1);
                            }
                        }
                        scene_dirty = 1;
                    }
                    if (layer_slot != 0) {
                        layer_surface_surface = layer_slot->surface;
                        layer_layer = layer_slot->layer;
                        layer_anchor = layer_slot->anchor;
                        for (unsigned index = 0; index < 4; index++) {
                            layer_margin[index] = layer_slot->margin[index];
                        }
                        if (committed_buffer != 0) {
                            layer_record_commit(layer_slot);
                            LayerEntry *stored =
                                layer_entry_by_surface(layer_slot->surface);
                            /* A surface stacked below a folded drag surface
                               (Phosh's quick-settings backdrop) that never
                               received an explicit alpha is invisible. Drop
                               its stored entry instead of relying on the
                               compose-time check: damage-only repaints would
                               otherwise keep the pixels it painted earlier. */
                            if (stored != 0 && stored->alpha_set == 0 &&
                                stored->below != 0) {
                                for (unsigned probe = 0;
                                     probe < MAX_DRAG_SURFACES; probe++) {
                                    DragSlot *below = &drag_slots[probe];
                                    if (below->used &&
                                        below->layer_surface == stored->below &&
                                        below->state == 0 &&
                                        !below->dragging) {
                                        layer_forget_surface(
                                            layer_slot->surface);
                                        stored = 0;
                                        break;
                                    }
                                }
                            }
                            if (stored != 0) {
                                int dx0, dy0, dx1, dy1;
                                take_damage((int)object, (int)buffer_width,
                                            (int)buffer_height,
                                            &dx0, &dy0, &dx1, &dy1);
                                int cx0, cy0, cx1, cy1;
                                if (layer_store_pixels(stored, dx0, dy0,
                                                       dx1, dy1, &cx0, &cy0,
                                                       &cx1, &cy1)) {
                                    int lx = 0;
                                    int ly = 0;
                                    layer_rect(stored, &lx, &ly);
                                    compose_damage(lx + cx0, ly + cy0,
                                                   lx + cx1, ly + cy1);
                                }
                            }
                            /* A draggable surface is positioned by the
                               compositor, not by its zwlr margin. */
                            drag_apply(layer_slot->layer_surface);
                        } else {
                            /* attach(NULL) hides the surface (Phosh does this
                               to the lockscreen after unlocking); it must
                               stop being drawn and stop taking input. */
                            layer_forget_surface(layer_slot->surface);
                        }
                        scene_dirty = 1;
                    }
#else
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
#endif
                }
                if (committed_buffer != 0) {
                    focus_surface = (layer_slot != 0)
                                        ? object
                                        : (surface_id != 0 ? surface_id : object);
                }
                if (committed_buffer != 0 || layer_slot != 0) {
                    /* Re-evaluate after every commit: a new surface may have
                       appeared on top, or the old top surface may have been
                       hidden. Routing is idempotent (no event unless the
                       target changes), so this also retries the first enter
                       until the seat proxies exist. */
                    pointer_route(client, pointer_x, pointer_y, 0);
                    keyboard_route(client);
                }
                if (committed_buffer != 0 && committed_fresh) {
                    begin(committed_buffer, 0 /* wl_buffer.release */);
                    finish();
                    send_message(client, -1);
                    /* Do NOT clear attached_buffers here: a commit without
                       attach keeps the current buffer. Clearing it made the
                       next commit look like attach(NULL), so GTK's repeated
                       present-without-attach dropped the whole layer. */
                }
                if (object < SURFACE_IDS && frame_ids[object] != 0) {
                    send_done(client, frame_ids[object], now_ms());
                    frame_ids[object] = 0;
                }
            }
            message_start += size;
        }
        /* Keep a truncated tail for the next recvmsg. Dropping it shifted
           every following message by a few bytes. */
        if (message_start < incoming_length) {
            unsigned long leftover = incoming_length - message_start;
            for (unsigned long index = 0; index < leftover; index++) {
                incoming[index] = incoming[message_start + index];
            }
            incoming_pending = leftover;
        } else {
            incoming_pending = 0;
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
static void reset_client(void);

/// Prepares a fresh protocol state for a new client without touching the
/// descriptors other clients still own.
static void init_client_state(int slot, long fd) {
    unsigned char *raw = (unsigned char *)&clients[slot];
    for (unsigned index = 0; index < sizeof(ClientState); index++) {
        raw[index] = 0;
    }
    for (unsigned index = 0; index < MAX_POOLS; index++) {
        clients[slot].pools[index].fd = -1;
    }
    for (unsigned index = 0; index < MAX_BUFFERS; index++) {
        clients[slot].buffers[index].pool_fd = -1;
    }
    clients[slot].pool_fd = -1;
    clients[slot].fd = fd;
    clients[slot].connected = 1;
    clients[slot].next_server_id = 0xFF000000u;

    current_client = slot;
    load_client(&clients[slot]);
    /* Clear the globals that are not part of ClientState. reset_client() also
       closes descriptors, and those are all -1 here. */
    int saved_dmabuf_fd[MAX_DMABUF_PARAMS];
    for (unsigned index = 0; index < MAX_DMABUF_PARAMS; index++) {
        saved_dmabuf_fd[index] = dmabuf_params[index].fd;
    }
    reset_client();
    for (unsigned index = 0; index < MAX_DMABUF_PARAMS; index++) {
        if (dmabuf_params[index].fd < 0) {
            dmabuf_params[index].fd = saved_dmabuf_fd[index];
        }
    }
    clients[slot].fd = fd;
    clients[slot].connected = 1;
    clients[slot].next_server_id = 0xFF000000u;
    save_client(&clients[slot]);
}

static void reset_client(void) {
    /* pools_reset() closes every descriptor owned by the pool table. */
    pool_fd = -1;
    pools_reset();
    attached_buffer = 0;
    registry_id = 0;
    compositor_id = 0;
    shm_id = 0;
    for (unsigned index = 0; index < MAX_SHM; index++) {
        shm_ids[index] = 0;
    }
    for (unsigned index = 0; index < MAX_DMABUF_PARAMS; index++) {
        if (dmabuf_params[index].fd >= 0) {
            sys3(57 /* close */, dmabuf_params[index].fd, 0, 0);
        }
        dmabuf_params[index].id = 0;
        dmabuf_params[index].fd = -1;
        dmabuf_params[index].has_plane = 0;
    }
    for (unsigned index = 0; index < 8; index++) {
        dmabuf_ids[index] = 0;
    }
    for (unsigned index = 0; index < 16; index++) {
        dmabuf_feedbacks[index] = 0;
    }
    dmabuf_id = 0;
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
    presentation_id = 0;
    feedback_id = 0;
    relative_manager_id = 0;
    relative_pointer_id = 0;
    constraints_id = 0;
    locked_id = 0;
    text_input_manager_id = 0;
    text_input_id = 0;
    layer_shell_id = 0;
    layer_surface_id = 0;
    layer_surface_surface = 0;
    layer_surface_seq = 0;
    for (unsigned index = 0; index < MAX_REGIONS; index++) {
        regions[index].id = 0;
    }
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        surface_input_set[index] = 0;
        surface_input_empty[index] = 0;
        attached_buffers[index] = 0;
        attached_fresh[index] = 0;
    }
    for (unsigned index = 0; index < MAX_LAYERS; index++) {
        /* The layer scene is global: a new client must not wipe the shell's
           layers. Only its own entries go away. */
        if (layer_entries[index].used &&
            layer_entries[index].owner != current_client) {
            continue;
        }
        if (layer_entries[index].copy != 0) {
            sys3(215 /* munmap */, (long)layer_entries[index].copy,
                 layer_entries[index].copy_bytes, 0);
            layer_entries[index].copy = 0;
            layer_entries[index].copy_bytes = 0;
            layer_entries[index].copy_ready = 0;
        }
        layer_entries[index].used = 0;
    }
    if (app_window.used && app_window.owner == current_client) {
        static const char reset[] = "gtk-serve: app reset\n";
        sys3(64, 1, (long)reset, sizeof(reset) - 1);
        foreign_toplevel_closed();
        app_window.used = 0;
        app_window.copy_ready = 0;
        app_window.toplevel = 0;
        app_surface_id = 0;
        app_window.x = 0;
        app_window.y = 0;
        app_window.hidden = 0;
    }
    for (unsigned index = 0; index < MAX_STACKED; index++) {
        stacked_slots[index].id = 0;
        stacked_slots[index].layer_surface = 0;
    }
    for (unsigned index = 0; index < MAX_LAYER_SURFACES; index++) {
        layer_surfaces[index].used = 0;
        layer_surfaces[index].alpha = 255;
    }
    for (unsigned index = 0; index < MAX_DRAG_SURFACES; index++) {
        drag_slots[index].used = 0;
        drag_slots[index].id = 0;
        drag_slots[index].layer_surface = 0;
    }
    drag_active = 0;
    for (unsigned index = 0; index < MAX_ALPHA_SURFACES; index++) {
        alpha_slots[index].used = 0;
        alpha_slots[index].id = 0;
        alpha_slots[index].layer_surface = 0;
    }
    layer_width = 0;
    layer_height = 0;
    layer_configured = 0;
    layer_acked = 0;
    text_input_enabled = 0;
    text_input_serial = 0;
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
    toplevel_surface_id = 0;
    popup_surface_id = 0;
    popup_xdg_surface_id = 0;
    idle_notifier_id = 0;
    idle_notification_id = 0;
    output_power_manager_id = 0;
    output_power_id = 0;
    layer_effects_id = 0;
    draggable_id = 0;
    alpha_id = 0;
    stacked_id = 0;
    foreign_toplevel_manager_id = 0;
    gamma_manager_id = 0;
    gamma_control_id = 0;
    output_manager_id = 0;
    output_config_id = 0;
    phosh_private_id = 0;
    startup_tracker_id = 0;
    keyboard_event_id = 0;
    xdg_switcher_id = 0;
    popup_id = 0;
    positioner_id = 0;
    popup_grab = 0;
    popup_committed = 0;
    pool_id = 0;
    buffer_id = 0;
    frame_id = 0;
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        frame_ids[index] = 0;
    }
    configure_serial = 1;
    verified = 0;
    commits = 0;
    focused = 0;
    for (unsigned index = 0; index < SURFACE_IDS; index++) {
        surface_entered[index] = 0;
    }
    /* The demo client expects the first enter at (100,50) logical. */
    focus_surface = 0;
    pointer_x = 200;
    pointer_y = 100;
    pointer_target = 0;
    keyboard_target = 0;
    pointer_grab = 0;
    selection_changed = 0;
    wants_offer = 0;
}

#ifdef SERVE_ONLY
/* GTK icon names are longer than the 27-byte SFS directory entry, so the icon
 * theme ships as one short-named archive and is unpacked into the ramfs. */
static unsigned char pack_buffer[64 * 1024];

static unsigned pack_u16(const unsigned char *data) {
    return (unsigned)data[0] | ((unsigned)data[1] << 8);
}

static unsigned pack_u32(const unsigned char *data) {
    return (unsigned)data[0] | ((unsigned)data[1] << 8) |
           ((unsigned)data[2] << 16) | ((unsigned)data[3] << 24);
}

static void make_dirs(char *path) {
    for (char *cursor = path + 1; *cursor != 0; cursor++) {
        if (*cursor == '/') {
            *cursor = 0;
            sys3(34 /* mkdirat */, -100, (long)path, 0777);
            *cursor = '/';
        }
    }
}

static void extract_theme(void) {
    long fd = sys3(56 /* openat */, -100, (long)"/sfs/theme.pack", 0);
    if (fd < 0) {
        return;
    }
    long count = sys3(63 /* read */, fd, (long)pack_buffer, sizeof(pack_buffer));
    sys3(57 /* close */, fd, 0, 0);
    if (count < 8 || pack_buffer[0] != 'W' || pack_buffer[1] != 'P' ||
        pack_buffer[2] != 'K' || pack_buffer[3] != '1') {
        return;
    }
    unsigned files = pack_u32(pack_buffer + 4);
    unsigned offset = 8;
    unsigned written = 0;
    for (unsigned index = 0; index < files; index++) {
        if (offset + 2 > (unsigned)count) {
            return;
        }
        unsigned name_length = pack_u16(pack_buffer + offset);
        offset += 2;
        if (offset + name_length + 4 > (unsigned)count) {
            return;
        }
        static const char root[] = "/tmp/xdg/";
        char path[160];
        unsigned base = 0;
        while (root[base] != 0) {
            path[base] = root[base];
            base++;
        }
        if (base + name_length + 1 >= sizeof(path)) {
            return;
        }
        for (unsigned byte = 0; byte < name_length; byte++) {
            path[base + byte] = (char)pack_buffer[offset + byte];
        }
        path[base + name_length] = 0;
        offset += name_length;
        unsigned data_length = pack_u32(pack_buffer + offset);
        offset += 4;
        if (offset + data_length > (unsigned)count) {
            return;
        }
        make_dirs(path);
        long out = sys3(56 /* openat */, -100, (long)path,
                        0x241 /* O_WRONLY|O_CREAT|O_TRUNC */);
        if (out >= 0) {
            sys3(64 /* write */, out, (long)(pack_buffer + offset), data_length);
            sys3(57 /* close */, out, 0, 0);
        }
        offset += data_length;
        written += 1;
    }
    static const char prefix[] = "theme: unpacked ";
    char text[sizeof(prefix) + 12];
    unsigned length = sizeof(prefix) - 1;
    for (unsigned index = 0; index < length; index++) {
        text[index] = prefix[index];
    }
    unsigned value = written;
    char digits[12];
    unsigned count_digits = 0;
    if (value == 0) {
        digits[count_digits++] = '0';
    }
    while (value != 0) {
        digits[count_digits++] = (char)('0' + value % 10);
        value /= 10;
    }
    while (count_digits != 0) {
        text[length++] = digits[--count_digits];
    }
    text[length++] = '\n';
    sys3(64, 1, (long)text, length);
}

void _start(void) {
    long server = sys3(198, 1, 1, 0);
    if (server < 0 || bind_path(server) < 0) {
        fail("gtk-serve: bind failed\n", 21);
    }
    if (sys3(201 /* listen */, server, 4, 0) < 0) {
        fail("gtk-serve: listen failed\n", 23);
    }
    {
        /* Host-shared display sink. The viewer maps the same 4 MiB file and
           expects the WIN1 header + RGBA pixels at 0x1000. */
        long shm = sys3(56 /* openat */, -100, (long)"/dev/ivshmem", 2 /* O_RDWR */);
        if (shm >= 0) {
            long mapped = sys6(222 /* mmap */, 0, 4 * 1024 * 1024,
                               3 /* RW */, 1 /* MAP_SHARED */, shm, 0);
            if (mapped > 0) {
                screen = (unsigned char *)mapped;
                /* Drop input events left in the shared ring by a previous
                   run; otherwise the first frame already reflects a stale
                   swipe. */
                *(volatile unsigned *)(screen + INPUT_HEAD) = 0;
                *(volatile unsigned *)(screen + INPUT_TAIL) = 0;
                static const char ready[] = "gtk-serve: screen mapped\n";
                sys3(64, 1, (long)ready, sizeof(ready) - 1);
            } else {
                static const char no[] = "gtk-serve: screen mmap failed\n";
                sys3(64, 1, (long)no, sizeof(no) - 1);
            }
            sys3(57 /* close */, shm, 0, 0);
        }
    }
    {
        /* The GPU scanout, when the machine has one. It is a plain pixel
           buffer (no WIN1 header); `ioctl` flushes it to the device. */
        long fd = sys3(56 /* openat */, -100, (long)"/dev/gpufb", 2 /* O_RDWR */);
        if (fd >= 0) {
            long mapped = sys6(222 /* mmap */, 0, 1024 * 768 * 4,
                               3 /* RW */, 1 /* MAP_SHARED */, fd, 0);
            if (mapped > 0) {
                gpufb = (unsigned char *)mapped;
                gpufb_fd = fd;
                static const char ready[] = "gtk-serve: gpu scanout mapped\n";
                sys3(64, 1, (long)ready, sizeof(ready) - 1);
            } else {
                sys3(57 /* close */, fd, 0, 0);
            }
        }
    }

    extract_theme();

    /* GLib's keyfile settings backend reads $XDG_CONFIG_HOME/glib-2.0/
       settings/keyfile before it falls back to schema defaults. There is no
       dconf/D-Bus in the guest, so seed the desktop defaults here: the
       wallpaper and the icon theme. Phosh reads them during startup. */
    sys3(34 /* mkdirat */, -100, (long)"/tmp/.config", 0777);
    sys3(34, -100, (long)"/tmp/.config/glib-2.0", 0777);
    sys3(34, -100, (long)"/tmp/.config/glib-2.0/settings", 0777);
    {
        long settings_fd = sys3(56 /* openat */, -100,
                                (long)"/tmp/.config/glib-2.0/settings/keyfile",
                                0x241 /* O_WRONLY|O_CREAT|O_TRUNC */);
        if (settings_fd >= 0) {
            static const char settings[] =
                "[org/gnome/desktop/background]\n"
                "picture-uri='file:///sfs/share/backgrounds/werewolf.png'\n"
                "picture-uri-dark='file:///sfs/share/backgrounds/werewolf.png'\n"
                "picture-options='zoom'\n"
                "primary-color='#0a0e1c'\n"
                "[org/gnome/desktop/screensaver]\n"
                "picture-uri='file:///sfs/share/backgrounds/werewolf.png'\n"
                "[org/gnome/desktop/interface]\n"
                "icon-theme='hicolor'\n"
                /* The image has /etc/passwd but no /etc/shadow and no PAM
                   stack, so Phosh's pam_unix authentication cannot succeed.
                   This desktop has no password yet: unlock on a key press or
                   a swipe, like a freshly provisioned device. */
                "[sm/puri/phosh/lockscreen]\n"
                "require-unlock=false\n"
                /* The app grid's home page lists favourites; with an empty
                   list the launcher is a blank page with only the search
                   entry. */
                "[org/gnome/shell]\n"
                "favorite-apps=['werewolf-terminal.desktop']\n";
            sys3(64 /* write */, settings_fd, (long)settings,
                 sizeof(settings) - 1);
            sys3(57 /* close */, settings_fd, 0, 0);
        }
    }

    /* Session bus for Phosh: libfeedback, polkit, the OSK manager and logind
       clients connect to it. /sfs/dbus-daemon ships with a permissive config;
       LD_LIBRARY_PATH is needed because its libsystemd has no RUNPATH. */
    long bus = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (bus == 0) {
        static const char *bus_argv[] = {
            "/sfs/dbus-daemon",
            "--config-file=/sfs/share/dbus-1/session.conf",
            "--nofork",
            "--nopidfile",
            0,
        };
        static const char *bus_envp[] = {
            "PATH=/sfs",
            "LD_LIBRARY_PATH=/sfs",
            0,
        };
        sys6(221 /* execve */, (long)bus_argv[0], (long)bus_argv,
             (long)bus_envp, 0, 0, 0);
        sys3(93 /* exit */, 0, 0, 0);
    }

    /* fontconfig only writes a cache into a directory that already exists */
    sys3(34 /* mkdirat */, -100, (long)"/tmp/fontcache", 0777);
    long child = sys6(220 /* clone */, 17 /* SIGCHLD */, 0, 0, 0, 0, 0);
    if (child < 0) {
        fail("gtk-serve: fork failed\n", 21);
    }
    if (child == 0) {
#ifdef SERVE_LAYER
        static const char *argv[] = {"/sfs/wlclient", "layer", 0};
#elif defined(SERVE_HIT)
        static const char *argv[] = {"/sfs/wlclient", "hit", 0};
#elif defined(SERVE_GTK4)
        static const char *argv[] = {"/sfs/gtk4hello", 0};
#elif defined(SERVE_WLGL)
        static const char *argv[] = {"/sfs/wlgltest", 0};
#elif defined(SERVE_TERM)
        static const char *argv[] = {"/sfs/term", 0};
#elif defined(SERVE_PHOSH)
        static const char *argv[] = {"/sfs/phosh", 0};
#else
        static const char *argv[] = {"/sfs/gtkdemo", 0};
#endif
        static const char *envp[] = {
            "XDG_RUNTIME_DIR=/tmp",
            "HOME=/tmp",
            "XKB_CONFIG_ROOT=/sfs/xkb",
            "WAYLAND_DISPLAY=wayland-test",
            "GDK_BACKEND=wayland",
            "LD_LIBRARY_PATH=/sfs",
            "XDG_DATA_DIRS=/tmp/xdg/share:/sfs/share",
            "GSETTINGS_SCHEMA_DIR=/sfs",
            "FONTCONFIG_FILE=/sfs/fonts.conf",
            "XDG_DATA_DIRS=/tmp/xdg/share:/sfs/share:/sfs/usr/share",
            "XCURSOR_THEME=default",
            "XCURSOR_PATH=/sfs/share/icons",
            "NO_AT_BRIDGE=1",
            /* The terminal and the shell paint through wl_shm; asking GDK for
               a GL context makes Mesa map llvmpipe and libLLVM (117-163 MB of
               libraries) before the first frame. */
            "GDK_GL=disable",
            "GDK_DEBUG=gl-disable",
#if defined(SERVE_GTK4) || defined(SERVE_WLGL)
            /* GTK4 renders through GSK; "gl" goes EGL -> virgl. The Mesa
               files live in the SFS root, and libgallium must be global so
               the DRI stub can resolve the driver entry points. */
            "GSK_RENDERER=gl",
            "LIBGL_DRIVERS_PATH=/sfs",
            "__EGL_VENDOR_LIBRARY_FILENAMES=/sfs/egl_vendor.json",
            "GBM_BACKENDS_PATH=/sfs",
            "LD_PRELOAD=/sfs/libgallium-25.0.7.so",
#else
            "GSK_RENDERER=cairo",
#endif
            "GTK_A11Y=none",
            /* GDesktopAppInfo execs gio-launch-desktop instead of forking the
               app itself; the desktop's libexec copy is not in this image. */
            "GIO_LAUNCH_DESKTOP=/sfs/gio-launch-desktop",
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/dbus-session",
            "XDG_CURRENT_DESKTOP=Phosh",
            "XDG_SESSION_TYPE=wayland",
            "XDG_SESSION_DESKTOP=phosh",
            /* glycin cannot create namespaces here; the loader itself is
             * still spawned directly and decodes normally. */
            "GLYCIN_DISABLE_SANDBOX=i-know-the-risks",
            /* glycin's zbus reactor and the executor tick loop starve each
             * other with the default worker count; four workers make the
             * loader reply deterministic (same workaround as glycintest). */
            "ASYNC_GLOBAL_EXECUTOR_THREADS=4",
            0,
        };
        sys6(221 /* execve */, (long)argv[0], (long)argv, (long)envp, 0, 0, 0);
        fail("gtk-serve: exec failed\n", 20);
    }
    sys3(64, 1, (long)"gtk-serve: waiting for client\n", 29);
    long client = sys3(202 /* accept */, server, 0, 0);
    if (client < 0) {
        fail("gtk-serve: accept failed\n", 23);
    }
    sys3(64, 1, (long)"gtk-serve: client connected\n", 28);
    init_client_state(0, client);
    int remaining = 1;

    /* Several clients: the shell (layer surfaces) plus applications it
       launches. Each keeps its own protocol state; the layer scene and the
       newest toplevel are global. */
    while (remaining > 0) {
        struct pollfd {
            int fd;
            short events;
            short revents;
        };
        struct pollfd ready[MAX_CLIENTS + 1];
        int slot_of[MAX_CLIENTS + 1];
        int count = 0;
        ready[count].fd = (int)server;
        ready[count].events = 1; /* POLLIN */
        ready[count].revents = 0;
        slot_of[count] = -1;
        count += 1;
        for (int index = 0; index < MAX_CLIENTS; index++) {
            if (!clients[index].connected) {
                continue;
            }
            ready[count].fd = (int)clients[index].fd;
            ready[count].events = 1;
            ready[count].revents = 0;
            slot_of[count] = index;
            count += 1;
        }
        struct { long seconds; long nanoseconds; } timeout = { 0, 20000000 };
        long ready_count = sys6(73 /* ppoll */, (long)ready, count,
                                (long)&timeout, 0, 0, 0);
        if (ready_count > 0) {
            if ((ready[0].revents & 1) != 0 && remaining < MAX_CLIENTS) {
                long accepted = sys3(202 /* accept */, server, 0, 0);
                if (accepted >= 0) {
                    int slot = -1;
                    for (int index = 0; index < MAX_CLIENTS; index++) {
                        if (!clients[index].connected) {
                            slot = index;
                            break;
                        }
                    }
                    if (slot < 0) {
                        sys3(57, accepted, 0, 0);
                    } else {
                        init_client_state(slot, accepted);
                        remaining += 1;
                        static const char joined[] =
                            "gtk-serve: client connected\n";
                        sys3(64, 1, (long)joined, sizeof(joined) - 1);
                    }
                }
            }
            for (int probe = 1; probe < count; probe++) {
                int index = slot_of[probe];
                if (index < 0 || !clients[index].connected) {
                    continue;
                }
                if ((ready[probe].revents & 1) == 0) {
                    continue;
                }
                current_client = index;
                load_client(&clients[index]);
                int result = serve(clients[index].fd);
                save_client(&clients[index]);
                if (result != 0) {
                    sys3(57, clients[index].fd, 0, 0);
                    clients[index].connected = 0;
                    remaining -= 1;
                    if (app_window.used && app_window.owner == index) {
                        static const char cleared[] = "gtk-serve: app cleared\n";
                        sys3(64, 1, (long)cleared, sizeof(cleared) - 1);
                        foreign_toplevel_closed();
                        app_window.used = 0;
                        app_window.copy_ready = 0;
                        app_window.toplevel = 0;
                        app_surface_id = 0;
                        app_window.x = 0;
                        app_window.y = 0;
                        app_window.hidden = 0;
                        window_drag = 0;
                    }
                    static const char left[] = "gtk-serve: client gone\n";
                    sys3(64, 1, (long)left, sizeof(left) - 1);
                }
            }
        }
        /* Only switch client state when the host actually queued input. */
        if (screen != 0) {
            unsigned input_head = *(volatile unsigned *)(screen + INPUT_HEAD);
            unsigned input_tail = *(volatile unsigned *)(screen + INPUT_TAIL);
            if (input_head != input_tail) {
                poll_input();
            }
        }
#ifdef SERVE_ONLY
        if (scene_dirty) {
            scene_dirty = 0;
            layer_compose();
        }
#endif
        /* Vblank tick: complete pending frame callbacks for every client. */
        for (int index = 0; index < MAX_CLIENTS; index++) {
            if (!clients[index].connected) {
                continue;
            }
            current_client = index;
            load_client(&clients[index]);
            for (unsigned surface = 0; surface < SURFACE_IDS; surface++) {
                if (frame_ids[surface] != 0) {
                    send_done(clients[index].fd, frame_ids[surface], now_ms());
                    frame_ids[surface] = 0;
                }
            }
            save_client(&clients[index]);
        }
    }
    for (int index = 0; index < MAX_CLIENTS; index++) {
        if (clients[index].connected) {
            sys3(57, clients[index].fd, 0, 0);
            clients[index].connected = 0;
        }
    }
    sys3(57, server, 0, 0);
    int status = 0;
    sys6(260 /* wait4 */, child, (long)&status, 0, 0, 0, 0);
    if ((status & 0x7f) != 0 || ((status >> 8) & 0xff) != 0) {
        fail("gtk-serve: client failed\n", 23);
    }
#if defined(SERVE_LAYER)
    sys3(64, 1, (long)"layer server ok\n", 16);
#elif defined(SERVE_HIT)
    sys3(64, 1, (long)"hit server ok\n", 14);
#elif defined(SERVE_GTK4)
    sys3(64, 1, (long)"gtk4 client ok\n", 15);
#elif defined(SERVE_WLGL)
    sys3(64, 1, (long)"wlgl client ok\n", 15);
#else
    sys3(64, 1, (long)"gtk client ok\n", 14);
#endif
    sys3(93, 0, 0, 0);
    for (;;) {
    }
}
#else
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
    /* The two wlclient roles above are the only clients this test starts. */
    for (int index = 0; index < 2; index++) {
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
    int remaining = 2;
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
    /* Only the two clients this test starts were served. */
    for (int index = 0; index < 2; index++) {
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
#endif

