/* A real libwayland client against the WerewolfOS Wayland transport.
 *
 *   wlclient            (executed by the test compositor)
 *
 * Builds with the distro's libwayland-client, connects to /tmp/wayland-test,
 * binds wl_compositor and wl_shm, creates a surface and an ARGB8888 buffer
 * from a memfd, commits it and round-trips. Prints "libwayland client ok".
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

static struct wl_compositor *compositor;
static struct wl_shm *shared_memory;

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    (void)data;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        compositor = wl_registry_bind(registry, name,
                                      &wl_compositor_interface, use);
    } else if (strcmp(interface, wl_shm_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        shared_memory = wl_registry_bind(registry, name, &wl_shm_interface, use);
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

static void shm_format(void *data, struct wl_shm *shm, uint32_t format) {
    (void)data;
    (void)shm;
    (void)format;
}

static const struct wl_shm_listener shm_listener = {
    .format = shm_format,
};

int main(void) {
    struct wl_display *display = wl_display_connect("/tmp/wayland-test");
    if (display == NULL) {
        fprintf(stderr, "wlclient: connect failed\n");
        return 2;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    if (registry == NULL) {
        return 3;
    }
    wl_registry_add_listener(registry, &registry_listener, NULL);
    if (wl_display_roundtrip(display) < 0) {
        return 4;
    }
    if (compositor == NULL || shared_memory == NULL) {
        fprintf(stderr, "wlclient: missing globals\n");
        return 5;
    }
    wl_shm_add_listener(shared_memory, &shm_listener, NULL);
    if (wl_display_roundtrip(display) < 0) {
        return 6;
    }

    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    if (surface == NULL) {
        return 7;
    }

    const int width = 8;
    const int height = 8;
    const int stride = 32;
    const int size = stride * height;
    int fd = memfd_create("wl-buffer", 0);
    if (fd < 0 || ftruncate(fd, size) != 0) {
        return 8;
    }
    uint32_t *pixels =
        mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) {
        return 9;
    }
    pixels[0] = 0xFF112233u;
    pixels[1] = 0xFF445566u;

    struct wl_shm_pool *pool = wl_shm_create_pool(shared_memory, fd, size);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(
        pool, 0, width, height, stride, WL_SHM_FORMAT_ARGB8888);
    if (pool == NULL || buffer == NULL) {
        return 10;
    }
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage(surface, 0, 0, width, height);
    wl_surface_commit(surface);

    if (wl_display_roundtrip(display) < 0) {
        return 11;
    }

    printf("libwayland client ok\n");
    fflush(stdout);
    wl_display_disconnect(display);
    return 0;
}
