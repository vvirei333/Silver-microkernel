/* Hand-written wayland-scanner output for wlr-screencopy-unstable-v1.
 * Pairs with tools/wlr-screencopy.h; the wire opcodes and signatures match
 * the upstream wlr-screencopy-unstable-v1.xml (version 3).
 */
#include "wayland-client.h"

extern const struct wl_interface wl_output_interface;
extern const struct wl_interface wl_buffer_interface;
extern const struct wl_interface zwlr_screencopy_manager_v1_interface;
extern const struct wl_interface zwlr_screencopy_frame_v1_interface;

static const struct wl_interface *frame_types[] = {
    &wl_buffer_interface, /* 0: buffer / copy / copy_with_damage */
    NULL, NULL, NULL, NULL, NULL, NULL, NULL,
};

static const struct wl_message zwlr_screencopy_frame_v1_requests[] = {
    { "copy", "o", frame_types + 0 },
    { "destroy", "", frame_types + 1 },
    { "copy_with_damage", "3o", frame_types + 0 },
};

static const struct wl_message zwlr_screencopy_frame_v1_events[] = {
    { "buffer", "n", frame_types + 0 },
    { "flags", "u", frame_types + 1 },
    { "ready", "2uuu", frame_types + 1 },
    { "failed", "", frame_types + 1 },
    { "damage", "3uuuu", frame_types + 1 },
    { "linux_dmabuf", "3h", frame_types + 1 },
    { "buffer_done", "3", frame_types + 1 },
};

const struct wl_interface zwlr_screencopy_frame_v1_interface = {
    "zwlr_screencopy_frame_v1", 3,
    3, zwlr_screencopy_frame_v1_requests,
    5, zwlr_screencopy_frame_v1_events,
};

static const struct wl_interface *manager_types[] = {
    &zwlr_screencopy_frame_v1_interface, /* 0: new frame */
    NULL,                                /* 1: int32 */
    &wl_output_interface,                /* 2: output */
    NULL, NULL,
};

static const struct wl_message zwlr_screencopy_manager_v1_requests[] = {
    { "capture_output", "nio", manager_types + 0 },
    { "capture_output_region", "nioiiii", manager_types + 0 },
    { "destroy", "", manager_types + 0 },
};

const struct wl_interface zwlr_screencopy_manager_v1_interface = {
    "zwlr_screencopy_manager_v1", 3,
    3, zwlr_screencopy_manager_v1_requests,
    0, NULL,
};
