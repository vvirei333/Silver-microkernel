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

static void frame_linux_dmabuf(void *data, struct zwlr_screencopy_frame_v1 *object,
                                int32_t fd) {
    (void)data; (void)object;
    close(fd); /* the pixman path never uses dmabuf */
}

static void frame_buffer_done(void *data, struct zwlr_screencopy_frame_v1 *object) {
    (void)data; (void)object;
}

static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    .buffer = frame_buffer,
    .flags = frame_flags,
    .ready = frame_ready_event,
    .failed = frame_failed_event,
    .damage = frame_damage,
    .linux_dmabuf = frame_linux_dmabuf,
    .buffer_done = frame_buffer_done,
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

/* Publish the captured frame into a file on the SFS root. The host reads
 * the same bytes straight out of the disk image (the guest writes them
 * through virtio-blk), so no ivshmem device is required.
 *
 * Layout: {magic, version, width, height, stride, format} then RGBA8888.
 */
#define SCANOUT_PATH "/frame.rgba"

static int publish(void) {
    int fd = open(SCANOUT_PATH, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open " SCANOUT_PATH);
        return -1;
    }
    uint32_t header[6] = {WINDOW_MAGIC, WINDOW_VERSION, frame_width,
                          frame_height, frame_width * 4, frame_format};
    if (write(fd, header, sizeof(header)) != (ssize_t)sizeof(header)) {
        perror("write header");
        close(fd);
        return -1;
    }
    const uint8_t *source = (const uint8_t *)pixels;
    int swap_red_blue = frame_format == WL_SHM_FORMAT_ARGB8888 ||
                        frame_format == WL_SHM_FORMAT_XRGB8888;
    uint8_t row_buffer[4096 * 4];
    for (uint32_t row = 0; row < frame_height; row++) {
        const uint8_t *src = source + (size_t)row * frame_stride;
        if (!swap_red_blue) {
            if (write(fd, src, (size_t)frame_width * 4) !=
                (ssize_t)(frame_width * 4)) {
                perror("write row");
                close(fd);
                return -1;
            }
        } else {
            for (uint32_t column = 0; column < frame_width; column++) {
                row_buffer[column * 4 + 0] = src[column * 4 + 2];
                row_buffer[column * 4 + 1] = src[column * 4 + 1];
                row_buffer[column * 4 + 2] = src[column * 4 + 0];
                row_buffer[column * 4 + 3] = src[column * 4 + 3];
            }
            if (write(fd, row_buffer, (size_t)frame_width * 4) !=
                (ssize_t)(frame_width * 4)) {
                perror("write row");
                close(fd);
                return -1;
            }
        }
    }
    fsync(fd);
    close(fd);
    frames_published++;
    if (frames_published % 5 == 1) {
        printf("SCREENCOPY FRAME %ux%u format=%u n=%d\n", frame_width,
               frame_height, frame_format, frames_published);
        fflush(stdout);
    }
    return 0;
}

static int capture_once(void) {
    if (manager == NULL || output == NULL || shm == NULL) {
        fprintf(stderr, "screencopy: missing manager/output/shm\n");
        return -1;
    }
    fprintf(stderr, "sc: capture\n");
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
    fprintf(stderr, "sc: copy\n");
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
    fprintf(stderr, "sc: connecting\n");
    display = wl_display_connect(name ? name : "wayland-0");
    if (!display) {
        fprintf(stderr, "screencopy: cannot connect (%s): %s\n",
                name ? name : "wayland-0", strerror(errno));
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    fprintf(stderr, "sc: roundtrip 1\n");
    if (wl_display_roundtrip(display) < 0) { fprintf(stderr, "sc: rt1 failed: %s\n", strerror(errno)); return 1; }
    fprintf(stderr, "sc: roundtrip 2\n");
    if (wl_display_roundtrip(display) < 0) { fprintf(stderr, "sc: rt2 failed: %s\n", strerror(errno)); return 1; }
    fprintf(stderr, "sc: globals manager=%p output=%p shm=%p\n", (void*)manager, (void*)output, (void*)shm);
    if (!manager || !output || !shm) {
        fprintf(stderr, "screencopy: missing globals (manager=%p output=%p shm=%p)\n",
                (void *)manager, (void *)output, (void *)shm);
        return 1;
    }
    for (;;) {
        capture_once();
        usleep(2 * 1000 * 1000);
    }
    return 0;
}
