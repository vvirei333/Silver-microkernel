/* Publish a headless Wayland compositor's output into the Silver ivshmem
 * window.
 *
 * phoc runs with the headless backend; this client asks the compositor for
 * wlr-screencopy frames, converts them to RGBA8888 and writes the window
 * protocol layout that the host viewer and `tools/demo.py` already read.
 *
 *   WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/tmp screencopy
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <wayland-client.h>

#include "wlr-screencopy.h"

#define WINDOW_MAGIC 0x57494E31u
#define WINDOW_VERSION 1u
#define WINDOW_HEADER 0x1000u
#define WINDOW_PIXELS 0x1000u
#define WINDOW_RGBA 1u
#define WINDOW_STATE_READY 1u

static struct wl_display *display;
static struct wl_shm *shm;
static struct wl_output *output;
static struct zwlr_screencopy_manager_v1 *manager;
static struct zwlr_screencopy_frame_v1 *frame;
static struct wl_buffer *buffer;
static void *pixels;
static size_t pixels_size;
static uint32_t frame_width, frame_height, frame_stride, frame_format;
static int have_buffer_info;
static int frame_ready;
static int frame_failed;
static int frames_published;

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    (void)data;
    if (strcmp(interface, wl_shm_interface.name) == 0) {
        shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (strcmp(interface, wl_output_interface.name) == 0) {
        if (!output) output = wl_registry_bind(registry, name, &wl_output_interface, 1);
    } else if (strcmp(interface, zwlr_screencopy_manager_v1_interface.name) == 0) {
        uint32_t bind_version = version < 3 ? version : 3;
        manager = wl_registry_bind(registry, name,
                                   &zwlr_screencopy_manager_v1_interface,
                                   bind_version);
    }
}

static void registry_remove(void *data, struct wl_registry *registry,
                            uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static void frame_buffer(void *data, struct zwlr_screencopy_frame_v1 *object,
                         uint32_t format, uint32_t width, uint32_t height,
                         uint32_t stride) {
    (void)data;
    (void)object;
    frame_format = format;
    frame_width = width;
    frame_height = height;
    frame_stride = stride;
    have_buffer_info = 1;
}

static void frame_flags(void *data, struct zwlr_screencopy_frame_v1 *object,
                        uint32_t flags) {
    (void)data;
    (void)object;
    (void)flags;
}

static void frame_damage(void *data, struct zwlr_screencopy_frame_v1 *object,
                         uint32_t x, uint32_t y, uint32_t width,
                         uint32_t height) {
    (void)data;
    (void)object;
    (void)x;
    (void)y;
    (void)width;
    (void)height;
}

static void frame_ready_event(void *data, struct zwlr_screencopy_frame_v1 *object,
                              uint32_t sec_hi, uint32_t sec_lo,
                              uint32_t nanoseconds) {
    (void)data;
    (void)object;
    (void)sec_hi;
    (void)sec_lo;
    (void)nanoseconds;
    frame_ready = 1;
}

static void frame_failed_event(void *data, struct zwlr_screencopy_frame_v1 *object) {
    (void)data;
    (void)object;
    frame_failed = 1;
}

static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    .buffer = frame_buffer,
    .flags = frame_flags,
    .ready = frame_ready_event,
    .failed = frame_failed_event,
    .damage = frame_damage,
};

static int create_shm_buffer(void) {
    if (!have_buffer_info || shm == NULL) return -1;
    int stride = (int)frame_stride;
    size_t size = (size_t)stride * frame_height;
    int fd = (int)syscall(SYS_memfd_create, "screencopy", 0);
    if (fd < 0) {
        perror("memfd_create");
        return -1;
    }
    if (ftruncate(fd, (off_t)size) != 0) {
        perror("ftruncate");
        close(fd);
        return -1;
    }
    void *mapped = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mapped == MAP_FAILED) {
        perror("mmap shm");
        close(fd);
        return -1;
    }
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int)size);
    buffer = wl_shm_pool_create_buffer(pool, 0, (int)frame_width,
                                       (int)frame_height, stride,
                                       frame_format);
    wl_shm_pool_destroy(pool);
    close(fd);
    if (!buffer) {
        munmap(mapped, size);
        return -1;
    }
    pixels = mapped;
    pixels_size = size;
    return 0;
}

static void destroy_frame(void) {
    if (frame) {
        zwlr_screencopy_frame_v1_destroy(frame);
        frame = NULL;
    }
    if (buffer) {
        wl_buffer_destroy(buffer);
        buffer = NULL;
    }
    if (pixels) {
        munmap(pixels, pixels_size);
        pixels = NULL;
    }
    have_buffer_info = 0;
}

static int publish(void) {
    int fd = open("/dev/ivshmem", O_RDWR);
    if (fd < 0) {
        perror("open /dev/ivshmem");
        return -1;
    }
    size_t map_size = WINDOW_HEADER + (size_t)frame_width * frame_height * 4;
    volatile uint8_t *window = mmap(NULL, map_size, PROT_READ | PROT_WRITE,
                                    MAP_SHARED, fd, 0);
    close(fd);
    if (window == MAP_FAILED) {
        perror("mmap ivshmem");
        return -1;
    }
    uint32_t *header = (uint32_t *)window;
    uint32_t sequence = header[0x18 / 4] + 1; /* odd: frame in progress */
    header[0x00 / 4] = WINDOW_MAGIC;
    header[0x04 / 4] = WINDOW_VERSION;
    header[0x08 / 4] = frame_width;
    header[0x0C / 4] = frame_height;
    header[0x10 / 4] = frame_width * 4;
    header[0x14 / 4] = WINDOW_RGBA;
    header[0x18 / 4] = sequence;
    header[0x1C / 4] = 0;
    __sync_synchronize();

    const uint8_t *source = (const uint8_t *)pixels;
    uint8_t *destination = (uint8_t *)window + WINDOW_PIXELS;
    int swap_red_blue = frame_format == WL_SHM_FORMAT_ARGB8888 ||
                        frame_format == WL_SHM_FORMAT_XRGB8888;
    for (uint32_t row = 0; row < frame_height; row++) {
        const uint8_t *src = source + (size_t)row * frame_stride;
        uint8_t *dst = destination + (size_t)row * frame_width * 4;
        if (!swap_red_blue) {
            memcpy(dst, src, (size_t)frame_width * 4);
        } else {
            for (uint32_t column = 0; column < frame_width; column++) {
                uint8_t b = src[column * 4 + 0];
                uint8_t g = src[column * 4 + 1];
                uint8_t r = src[column * 4 + 2];
                uint8_t a = src[column * 4 + 3];
                dst[column * 4 + 0] = r;
                dst[column * 4 + 1] = g;
                dst[column * 4 + 2] = b;
                dst[column * 4 + 3] = a;
            }
        }
    }
    __sync_synchronize();
    header[0x18 / 4] = sequence + 1; /* even: stable */
    header[0x1C / 4] = WINDOW_STATE_READY;
    header[0x2C / 4] = 1; /* show the host window */
    munmap((void *)window, map_size);
    frames_published++;
    if (frames_published == 1) {
        printf("SCREENCOPY FRAME %ux%u format=%u\n", frame_width, frame_height,
               frame_format);
        fflush(stdout);
    }
    return 0;
}

static int capture_once(void) {
    if (manager == NULL || output == NULL || shm == NULL) {
        fprintf(stderr, "screencopy: missing manager/output/shm\n");
        return -1;
    }
    frame = zwlr_screencopy_manager_v1_capture_output(manager, 0, output);
    if (!frame) return -1;
    zwlr_screencopy_frame_v1_add_listener(frame, &frame_listener, NULL);
    have_buffer_info = 0;
    frame_ready = 0;
    frame_failed = 0;
    while (!have_buffer_info && !frame_failed) {
        if (wl_display_dispatch(display) < 0) return -1;
    }
    if (frame_failed) return -1;
    if (create_shm_buffer() != 0) return -1;
    zwlr_screencopy_frame_v1_copy(frame, buffer);
    while (!frame_ready && !frame_failed) {
        if (wl_display_dispatch(display) < 0) return -1;
    }
    if (frame_failed) {
        destroy_frame();
        return -1;
    }
    int result = publish();
    destroy_frame();
    return result;
}

int main(void) {
    const char *name = getenv("WAYLAND_DISPLAY");
    display = wl_display_connect(name ? name : "wayland-0");
    if (!display) {
        fprintf(stderr, "screencopy: cannot connect (%s): %s\n",
                name ? name : "wayland-0", strerror(errno));
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    wl_display_roundtrip(display);
    if (!manager || !output || !shm) {
        fprintf(stderr, "screencopy: missing globals (manager=%p output=%p shm=%p)\n",
                (void *)manager, (void *)output, (void *)shm);
        return 1;
    }
    for (;;) {
        capture_once();
        usleep(100 * 1000);
    }
    return 0;
}
