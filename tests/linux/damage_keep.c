/* damage_keep.c -- preserve undamaged pixels for wl_shm clients.
 *
 * wlroots 0.20's pixman renderer has no `update_from_buffer` implementation
 * (render/pixman/renderer.c only implements `texture_from_buffer`, which wraps
 * the client's shared memory). A compositor is required to keep a surface's
 * previous content outside the region the client marked with
 * `wl_surface.damage`; the pixman renderer instead shows the raw contents of
 * the newly attached buffer there.
 *
 * GDK3 only paints the dirty region into a new shm buffer and lets the
 * compositor preserve the rest, so under phoc every partially damaged commit
 * turns the untouched area black: the app grid goes black when the search
 * field takes focus, the terminal goes black once it repaints a few rows.
 * GLES (the renderer real phones use) implements `update_from_buffer` and
 * copies the damage into the previous texture, which is why this only shows
 * up in the nested/pixman setup.
 *
 * This preloaded object restores the missing behaviour on the client side: it
 * tracks shm pools/buffers and the damage each surface advertises, and before
 * `wl_surface.commit` copies the previous buffer's pixels into every region
 * the client did not repaint. The compositor then sees a complete buffer.
 *
 * The hook is `wl_proxy_marshal_array_flags`: the protocol wrappers in
 * wayland-client-protocol.h expand to `wl_proxy_marshal_flags`, which is
 * variadic and forwards to the array variant through the PLT. The array
 * variant hands us the arguments in a plain `union wl_argument *`.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

struct wl_proxy;
struct wl_interface;
struct wl_shm_pool;

typedef void *(*marshal_array_fn)(struct wl_proxy *, uint32_t,
                                  const struct wl_interface *, uint32_t,
                                  uint32_t, const intptr_t *);

extern void *wl_proxy_marshal_array_flags(struct wl_proxy *proxy,
                                          uint32_t opcode,
                                          const struct wl_interface *interface,
                                          uint32_t version, uint32_t flags,
                                          const intptr_t *args);
extern const char *wl_proxy_get_class(struct wl_proxy *proxy);

static marshal_array_fn real_marshal;

#define MAX_RECTS 64
/* Old pools have to stay mapped until the surface that used them commits
   again; a small ring bounds the retained memory (GTK allocates ~3 MiB per
   buffer at 1024x768). */
#define MAX_DEAD_POOLS 6

struct pool {
    struct wl_proxy *proxy;
    unsigned char *map;
    size_t size;
    int dead;
    struct pool *next;
};

struct buffer {
    struct wl_proxy *proxy;
    struct pool *pool;
    int32_t offset;
    int32_t width;
    int32_t height;
    int32_t stride;
    uint32_t format;
    struct buffer *next;
};

struct rect {
    int32_t x, y, width, height;
};

struct surface {
    struct wl_proxy *proxy;
    int scale;
    struct buffer *pending;
    struct buffer *committed;
    /* Pixels of the last committed frame, independent of the buffer the
       client used (GDK frees the pool as soon as the compositor releases the
       buffer). */
    unsigned char *snapshot;
    size_t snapshot_size;
    struct buffer *snapshot_buffer;
    struct rect damage[MAX_RECTS];
    int damage_count;
    int damage_overflow;
    struct surface *next;
};

static struct pool *pools;
static struct buffer *buffers;
static struct surface *surfaces;
static int dead_pools;
static int verbose;

static void logmsg(const char *format, ...) {
    if (!verbose) {
        return;
    }
    char buffer[256];
    va_list ap;
    va_start(ap, format);
    int length = vsnprintf(buffer, sizeof(buffer), format, ap);
    va_end(ap);
    if (length <= 0) {
        return;
    }
    if (length > (int)sizeof(buffer) - 1) {
        length = (int)sizeof(buffer) - 1;
    }
    (void)!write(2, buffer, (size_t)length);
}

static struct pool *find_pool(struct wl_proxy *proxy) {
    for (struct pool *pool = pools; pool != NULL; pool = pool->next) {
        if (pool->proxy == proxy) {
            return pool;
        }
    }
    return NULL;
}

static struct buffer *find_buffer(struct wl_proxy *proxy) {
    if (proxy == NULL) {
        return NULL;
    }
    for (struct buffer *buffer = buffers; buffer != NULL; buffer = buffer->next) {
        if (buffer->proxy == proxy) {
            return buffer;
        }
    }
    return NULL;
}

static struct surface *find_surface(struct wl_proxy *proxy) {
    for (struct surface *surface = surfaces; surface != NULL;
         surface = surface->next) {
        if (surface->proxy == proxy) {
            return surface;
        }
    }
    struct surface *surface = calloc(1, sizeof(*surface));
    if (surface == NULL) {
        return NULL;
    }
    surface->proxy = proxy;
    surface->scale = 1;
    surface->next = surfaces;
    surfaces = surface;
    return surface;
}

static int pool_referenced(struct pool *pool) {
    for (struct surface *surface = surfaces; surface != NULL;
         surface = surface->next) {
        if ((surface->committed != NULL &&
             surface->committed->pool == pool) ||
            (surface->pending != NULL && surface->pending->pool == pool)) {
            return 1;
        }
    }
    return 0;
}

static void trim_dead_pools(void) {
    while (dead_pools > MAX_DEAD_POOLS) {
        /* Never drop a pool that a surface still needs as its previous
           frame: GTK destroys the pool as soon as the compositor releases the
           buffer, but the pixels are still the source for the next partial
           damage. */
        struct pool **oldest = NULL;
        for (struct pool **link = &pools; *link != NULL; link = &(*link)->next) {
            if ((*link)->dead && (*link)->map != NULL &&
                !pool_referenced(*link)) {
                oldest = link;
            }
        }
        if (oldest == NULL) {
            return;
        }
        struct pool *pool = *oldest;
        *oldest = pool->next;
        if (pool->map != NULL) {
            munmap(pool->map, pool->size);
        }
        /* Any surface still pointing at this pool must stop seeing it. */
        pool->map = NULL;
        pool->size = 0;
        free(pool);
        dead_pools--;
    }
}

static void record_pool(struct wl_proxy *proxy, int fd, int32_t size) {
    if (proxy == NULL || size <= 0 || fd < 0) {
        return;
    }
    struct pool *pool = calloc(1, sizeof(*pool));
    if (pool == NULL) {
        return;
    }
    void *map = mmap(NULL, (size_t)size, PROT_READ | PROT_WRITE, MAP_SHARED,
                     fd, 0);
    if (map == MAP_FAILED) {
        logmsg("damage_keep: pool mmap failed fd=%d size=%d errno=%d\n", fd,
               (int)size, errno);
        free(pool);
        return;
    }
    pool->proxy = proxy;
    pool->map = map;
    pool->size = (size_t)size;
    pool->next = pools;
    pools = pool;
}

static void record_buffer(struct wl_proxy *proxy,
                          struct wl_shm_pool *pool_proxy, int32_t offset,
                          int32_t width, int32_t height, int32_t stride,
                          uint32_t format) {
    if (proxy == NULL || width <= 0 || height <= 0 || stride <= 0 || offset < 0) {
        return;
    }
    /* Only the 4-byte packed formats GTK uses for windows. */
    if (format != 0 /* argb8888 */ && format != 1 /* xrgb8888 */) {
        return;
    }
    struct pool *pool = find_pool((struct wl_proxy *)pool_proxy);
    if (pool == NULL) {
        return;
    }
    struct buffer *buffer = calloc(1, sizeof(*buffer));
    if (buffer == NULL) {
        return;
    }
    buffer->proxy = proxy;
    buffer->pool = pool;
    buffer->offset = offset;
    buffer->width = width;
    buffer->height = height;
    buffer->stride = stride;
    buffer->format = format;
    buffer->next = buffers;
    buffers = buffer;
}

static int buffer_mapped(struct buffer *buffer) {
    if (buffer == NULL || buffer->pool == NULL || buffer->pool->map == NULL) {
        return 0;
    }
    size_t needed = (size_t)buffer->offset +
                    (size_t)buffer->stride * (size_t)buffer->height;
    return needed <= buffer->pool->size;
}

static unsigned char *buffer_data(struct buffer *buffer) {
    return buffer->pool->map + buffer->offset;
}

/* Copies the previous buffer's pixels into @to wherever the client did not
   paint this frame. */
static void preserve_contents(struct surface *surface, const unsigned char *source,
                              int source_stride, struct buffer *to) {
    if (!buffer_mapped(to) || source == NULL) {
        return;
    }
    int width = to->width;
    int height = to->height;
    unsigned char *destination = buffer_data(to);
    int destination_stride = to->stride;

    for (int y = 0; y < height; y++) {
        int damaged[MAX_RECTS * 2];
        int intervals = 0;
        for (int index = 0; index < surface->damage_count; index++) {
            struct rect *rect = &surface->damage[index];
            if (y < rect->y || y >= rect->y + rect->height) {
                continue;
            }
            int start = rect->x;
            int end = rect->x + rect->width;
            if (start < 0) {
                start = 0;
            }
            if (end > width) {
                end = width;
            }
            if (start >= end) {
                continue;
            }
            if (intervals >= MAX_RECTS) {
                break;
            }
            /* insertion sort by start */
            int insert = intervals;
            while (insert > 0 && damaged[(insert - 1) * 2] > start) {
                damaged[insert * 2] = damaged[(insert - 1) * 2];
                damaged[insert * 2 + 1] = damaged[(insert - 1) * 2 + 1];
                insert--;
            }
            damaged[insert * 2] = start;
            damaged[insert * 2 + 1] = end;
            intervals++;
        }

        int cursor = 0;
        for (int index = 0; index < intervals; index++) {
            int start = damaged[index * 2];
            int end = damaged[index * 2 + 1];
            if (end <= cursor) {
                continue;
            }
            if (start > cursor) {
                memcpy(destination + (size_t)y * destination_stride +
                           (size_t)cursor * 4,
                       source + (size_t)y * source_stride + (size_t)cursor * 4,
                       (size_t)(start - cursor) * 4);
            }
            if (end > cursor) {
                cursor = end;
            }
        }
        if (cursor < width) {
            memcpy(destination + (size_t)y * destination_stride +
                       (size_t)cursor * 4,
                   source + (size_t)y * source_stride + (size_t)cursor * 4,
                   (size_t)(width - cursor) * 4);
        }
    }
}

/* Non-zero pixel count, for diagnosing whether the buffer we commit still
   has holes. */
__attribute__((unused)) static unsigned long painted_pixels(struct buffer *buffer) {
    if (!buffer_mapped(buffer)) {
        return 0;
    }
    unsigned char *data = buffer_data(buffer);
    unsigned long count = 0;
    for (int32_t y = 0; y < buffer->height; y++) {
        unsigned char *row = data + (size_t)y * buffer->stride;
        for (int32_t x = 0; x < buffer->width; x++) {
            if (row[x * 4] != 0 || row[x * 4 + 1] != 0 || row[x * 4 + 2] != 0) {
                count++;
            }
        }
    }
    return count;
}

/* Copies the painted regions from the live buffer into the snapshot. */
static void snapshot_damage(struct surface *surface, struct buffer *buffer) {
    if (surface->snapshot == NULL || !buffer_mapped(buffer)) {
        return;
    }
    unsigned char *data = buffer_data(buffer);
    for (int index = 0; index < surface->damage_count; index++) {
        struct rect *rect = &surface->damage[index];
        int x0 = rect->x < 0 ? 0 : rect->x;
        int y0 = rect->y < 0 ? 0 : rect->y;
        int x1 = rect->x + rect->width;
        int y1 = rect->y + rect->height;
        if (x1 > buffer->width) {
            x1 = buffer->width;
        }
        if (y1 > buffer->height) {
            y1 = buffer->height;
        }
        for (int y = y0; y < y1; y++) {
            memcpy(surface->snapshot + (size_t)y * buffer->stride + (size_t)x0 * 4,
                   data + (size_t)y * buffer->stride + (size_t)x0 * 4,
                   (size_t)(x1 - x0) * 4);
        }
    }
}

static int snapshot_allocate(struct surface *surface, struct buffer *buffer) {
    size_t needed = (size_t)buffer->stride * (size_t)buffer->height;
    if (surface->snapshot != NULL && surface->snapshot_size == needed) {
        return 1;
    }
    unsigned char *snapshot = realloc(surface->snapshot, needed);
    if (snapshot == NULL) {
        return 0;
    }
    surface->snapshot = snapshot;
    surface->snapshot_size = needed;
    return 1;
}

static void add_damage(struct surface *surface, int32_t x, int32_t y,
                       int32_t width, int32_t height, int scale) {
    if (width <= 0 || height <= 0) {
        return;
    }
    if (surface->damage_count >= MAX_RECTS) {
        /* Too many rectangles: we cannot tell what was painted, so leave the
           buffer alone rather than risk clobbering new content. */
        surface->damage_overflow = 1;
        return;
    }
    struct rect *rect = &surface->damage[surface->damage_count++];
    rect->x = x * scale;
    rect->y = y * scale;
    rect->width = width * scale;
    rect->height = height * scale;
}

static void handle_request(struct wl_proxy *proxy, uint32_t opcode,
                           const char *class, const intptr_t *args,
                           void *result) {
    if (strcmp(class, "wl_shm") == 0) {
        if (opcode == 0 /* create_pool */) {
            /* create_pool(new_id, fd, size): the new id is allocated by
               libwayland, so the caller passes NULL for it. */
            record_pool((struct wl_proxy *)result, (int)args[1],
                        (int32_t)args[2]);
        }
        return;
    }

    if (strcmp(class, "wl_shm_pool") == 0) {
        if (opcode == 0 /* create_buffer */) {
            record_buffer((struct wl_proxy *)result,
                          (struct wl_shm_pool *)proxy, (int32_t)args[1],
                          (int32_t)args[2], (int32_t)args[3], (int32_t)args[4],
                          (uint32_t)args[5]);
        } else if (opcode == 1 /* destroy */) {
            struct pool *pool = find_pool(proxy);
            if (pool != NULL && !pool->dead) {
                pool->dead = 1;
                dead_pools++;
                trim_dead_pools();
            }
        }
        return;
    }

    if (strcmp(class, "wl_surface") != 0) {
        return;
    }

    struct surface *surface = find_surface(proxy);
    if (surface == NULL) {
        return;
    }

    switch (opcode) {
    case 1: /* attach */
        if (args[0] == 0) {
            surface->pending = NULL;
            surface->committed = NULL;
        } else {
            surface->pending = find_buffer((struct wl_proxy *)args[0]);
        }
        break;
    case 2: /* damage (surface coordinates) */
        add_damage(surface, (int32_t)args[0], (int32_t)args[1],
                   (int32_t)args[2], (int32_t)args[3], surface->scale);
        break;
    case 9: /* damage_buffer */
        add_damage(surface, (int32_t)args[0], (int32_t)args[1],
                   (int32_t)args[2], (int32_t)args[3], 1);
        break;
    case 8: /* set_buffer_scale */
        surface->scale = (int32_t)args[0] > 0 ? (int32_t)args[0] : 1;
        break;
    case 6: /* commit */
        if (surface->pending != NULL && surface->pending->pool != NULL &&
            buffer_mapped(surface->pending)) {
            struct buffer *buffer = surface->pending;
            size_t needed = (size_t)buffer->stride * (size_t)buffer->height;
            int same_size = surface->snapshot != NULL &&
                            surface->snapshot_size == needed;
            if (same_size && surface->snapshot_buffer != buffer) {
                /* A new buffer: GDK painted only the damage into it, so the
                   rest still holds whatever shared memory came from (usually
                   zeroes). Fill it from the previous frame. */
                if (!surface->damage_overflow) {
                    logmsg("damage_keep: fill %dx%d damage=%d\n", buffer->width,
                           buffer->height, surface->damage_count);
                    preserve_contents(surface, surface->snapshot, buffer->stride,
                                      buffer);
                }
                memcpy(surface->snapshot, buffer_data(buffer), needed);
                surface->snapshot_buffer = buffer;
            } else if (same_size) {
                /* Same buffer reused: the client painted only the damage,
                   the snapshot already holds the rest. */
                snapshot_damage(surface, buffer);
            } else if (snapshot_allocate(surface, buffer)) {
                memcpy(surface->snapshot, buffer_data(buffer), needed);
                surface->snapshot_buffer = buffer;
            }
            surface->committed = buffer;
        }
        surface->pending = NULL;
        surface->damage_count = 0;
        surface->damage_overflow = 0;
        break;
    default:
        break;
    }
}

void *wl_proxy_marshal_array_flags(struct wl_proxy *proxy, uint32_t opcode,
                                   const struct wl_interface *interface,
                                   uint32_t version, uint32_t flags,
                                   const intptr_t *args) {
    if (real_marshal == NULL) {
        real_marshal = (marshal_array_fn)dlsym(RTLD_NEXT,
                                               "wl_proxy_marshal_array_flags");
        if (real_marshal == NULL) {
            abort();
        }
        verbose = getenv("DAMAGE_KEEP_VERBOSE") != NULL;
        logmsg("damage_keep: active\n");
    }

    if (proxy == NULL) {
        return real_marshal(proxy, opcode, interface, version, flags, args);
    }

    const char *class = wl_proxy_get_class(proxy);

    /* Constructors hand us the new proxy in the return value, so their state
       can only be recorded after the call. Every other request has to be
       handled first: commit must see the pixels restored before it reaches
       the socket, and wl_shm_pool.destroy frees its proxy. */
    int constructor = class != NULL &&
        ((strcmp(class, "wl_shm") == 0 && opcode == 0) ||
         (strcmp(class, "wl_shm_pool") == 0 && opcode == 0));

    if (!constructor && class != NULL) {
        handle_request(proxy, opcode, class, args, NULL);
    }

    void *result = real_marshal(proxy, opcode, interface, version, flags, args);

    if (constructor && class != NULL) {
        handle_request(proxy, opcode, class, args, result);
    }
    return result;
}
