/* Hand-written wlr-screencopy-unstable-v1 bindings (wayland-scanner style).
 *
 * The build machine has no wayland-scanner; the protocol is small and its
 * wire opcodes are stable:
 *   zwlr_screencopy_manager_v1: destroy(0), capture_output(1),
 *                               capture_output_region(2)
 *   zwlr_screencopy_frame_v1 events: buffer(0), flags(1), ready(2),
 *                                    failed(3), damage(4, v3)
 *   zwlr_screencopy_frame_v1 requests: copy(0), destroy(1),
 *                                      copy_with_damage(2, v3)
 */
#ifndef WLR_SCREENCOPY_H
#define WLR_SCREENCOPY_H

#include <stdint.h>
#include <stddef.h>
#include "wayland-client.h"

struct wl_output;
struct wl_buffer;
struct zwlr_screencopy_manager_v1;
struct zwlr_screencopy_frame_v1;

extern const struct wl_interface zwlr_screencopy_manager_v1_interface;
extern const struct wl_interface zwlr_screencopy_frame_v1_interface;

struct zwlr_screencopy_frame_v1_listener {
    void (*buffer)(void *data, struct zwlr_screencopy_frame_v1 *frame,
                   uint32_t format, uint32_t width, uint32_t height,
                   uint32_t stride);
    void (*flags)(void *data, struct zwlr_screencopy_frame_v1 *frame,
                  uint32_t flags);
    void (*ready)(void *data, struct zwlr_screencopy_frame_v1 *frame,
                  uint32_t tv_sec_hi, uint32_t tv_sec_lo, uint32_t tv_nsec);
    void (*failed)(void *data, struct zwlr_screencopy_frame_v1 *frame);
    void (*damage)(void *data, struct zwlr_screencopy_frame_v1 *frame,
                   uint32_t x, uint32_t y, uint32_t width, uint32_t height);
    void (*linux_dmabuf)(void *data, struct zwlr_screencopy_frame_v1 *frame,
                         int32_t fd);
    void (*buffer_done)(void *data, struct zwlr_screencopy_frame_v1 *frame);
};

static inline int
zwlr_screencopy_frame_v1_add_listener(struct zwlr_screencopy_frame_v1 *frame,
                                      const struct zwlr_screencopy_frame_v1_listener *listener,
                                      void *data) {
    return wl_proxy_add_listener((struct wl_proxy *) frame,
                                 (void (**)(void)) listener, data);
}

static inline void
zwlr_screencopy_manager_v1_destroy(struct zwlr_screencopy_manager_v1 *manager) {
    wl_proxy_marshal((struct wl_proxy *) manager, 2);
    wl_proxy_destroy((struct wl_proxy *) manager);
}

static inline struct zwlr_screencopy_frame_v1 *
zwlr_screencopy_manager_v1_capture_output(struct zwlr_screencopy_manager_v1 *manager,
                                          int32_t overlay_cursor,
                                          struct wl_output *output) {
    return (struct zwlr_screencopy_frame_v1 *) wl_proxy_marshal_constructor(
        (struct wl_proxy *) manager, 0,
        &zwlr_screencopy_frame_v1_interface, overlay_cursor, output);
}

static inline struct zwlr_screencopy_frame_v1 *
zwlr_screencopy_manager_v1_capture_output_region(
        struct zwlr_screencopy_manager_v1 *manager, int32_t overlay_cursor,
        struct wl_output *output, int32_t x, int32_t y, int32_t width,
        int32_t height) {
    return (struct zwlr_screencopy_frame_v1 *) wl_proxy_marshal_constructor(
        (struct wl_proxy *) manager, 1,
        &zwlr_screencopy_frame_v1_interface, overlay_cursor, output,
        x, y, width, height);
}

static inline void
zwlr_screencopy_frame_v1_copy(struct zwlr_screencopy_frame_v1 *frame,
                              struct wl_buffer *buffer) {
    wl_proxy_marshal((struct wl_proxy *) frame, 0, buffer);
}

static inline void
zwlr_screencopy_frame_v1_destroy(struct zwlr_screencopy_frame_v1 *frame) {
    wl_proxy_marshal((struct wl_proxy *) frame, 1);
    wl_proxy_destroy((struct wl_proxy *) frame);
}

static inline void
zwlr_screencopy_frame_v1_copy_with_damage(struct zwlr_screencopy_frame_v1 *frame,
                                          struct wl_buffer *buffer) {
    wl_proxy_marshal((struct wl_proxy *) frame, 2, buffer);
}

#endif
