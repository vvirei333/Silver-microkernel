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
#include "xdg-shell-client-protocol.h"
#include "xdg-decoration-client-protocol.h"
#include "viewporter-client-protocol.h"
#include "xdg-output-client-protocol.h"
#include "xdg-activation-client-protocol.h"
#include "fractional-scale-client-protocol.h"
#include "presentation-time-client-protocol.h"
#include "relative-pointer-client-protocol.h"
#include "pointer-constraints-client-protocol.h"
#include "text-input-client-protocol.h"

static struct wl_compositor *compositor;
static struct wl_shm *shared_memory;
static struct xdg_wm_base *shell;
static struct wl_output *output;
static struct wl_seat *seat;
static struct wl_data_device_manager *data_manager;
static struct zxdg_decoration_manager_v1 *decoration_manager;
static struct zxdg_toplevel_decoration_v1 *decoration;
static struct wp_viewporter *viewporter;
static struct wp_viewport *viewport;
static struct wl_subcompositor *subcompositor;
static struct wl_subsurface *subsurface;
static struct zxdg_output_manager_v1 *xdg_output_manager;
static struct zxdg_output_v1 *xdg_output;
static int xdg_output_ok;
static struct xdg_activation_v1 *activation;
static struct xdg_activation_token_v1 *activation_token;
static int activation_token_ok;
static struct wp_fractional_scale_manager_v1 *fractional_manager;
static struct wp_fractional_scale_v1 *fractional_scale;
static int fractional_ok;
static struct wp_presentation *presentation;
static struct wp_presentation_feedback *presentation_feedback;
static int presentation_clock_ok;
static int presentation_ok;
static struct zwp_relative_pointer_manager_v1 *relative_manager;
static struct zwp_relative_pointer_v1 *relative_pointer;
static int relative_ok;
static struct zwp_pointer_constraints_v1 *constraints;
static struct zwp_locked_pointer_v1 *locked_pointer;
static int locked_ok;
static struct zwp_text_input_manager_v3 *text_input_manager;
static struct zwp_text_input_v3 *text_input;
static int text_input_entered;
static int text_input_committed;
static int text_input_done;
static struct wl_data_device *data_device;
static struct wl_data_source *data_source;
static struct wl_data_offer *clipboard_offer;
static int clipboard_selection;
static int clipboard_source_sent;
static int clipboard_mime_ok;
static int decoration_client_side;
static int decoration_configures;
static struct wl_keyboard *keyboard;
static struct wl_pointer *pointer;
static uint32_t configure_serial;
static int configured;
static int output_done;
static int toplevel_maximized;
static int toplevel_fullscreen;
static int output_scale;
static int surface_entered;
static int popup_configured;
static int popup_configure_ok;
static int popup_done;
static int output_name_ok;
static int output_description_ok;
static int seat_name_ok;
static int keyboard_repeat_ok;
static uint32_t seat_caps;
static int keymap_ok;
static int pointer_ok;
static int key_enter;
static int key_left;
static int pointer_left;
static int frame_done;
static int buffer_releases;

static void output_geometry(void *data, struct wl_output *out, int32_t x,
                            int32_t y, int32_t physical_width,
                            int32_t physical_height, int32_t subpixel,
                            const char *make, const char *model,
                            int32_t transform);
static void output_mode(void *data, struct wl_output *out, uint32_t flags,
                        int32_t width, int32_t height, int32_t refresh);
static void output_done_event(void *data, struct wl_output *out);
static void output_scale_event(void *data, struct wl_output *out, int32_t factor);
static void output_name_event(void *data, struct wl_output *out, const char *name);
static void output_description_event(void *data, struct wl_output *out,
                                     const char *description);
static const struct wl_output_listener output_listener = {
    .geometry = output_geometry,
    .mode = output_mode,
    .done = output_done_event,
    .scale = output_scale_event,
    .name = output_name_event,
    .description = output_description_event,
};
static void seat_capabilities(void *data, struct wl_seat *s, uint32_t caps);
static void seat_name_event(void *data, struct wl_seat *s, const char *name);
static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities,
    .name = seat_name_event,
};

static const char clipboard_text[] = "clipboard from werewolf\n";

static void data_source_target(void *data, struct wl_data_source *source,
                               const char *mime_type) {
    (void)data; (void)source; (void)mime_type;
}

static void data_source_send(void *data, struct wl_data_source *source,
                             const char *mime_type, int32_t fd) {
    (void)data; (void)source;
    if (mime_type != NULL &&
        strcmp(mime_type, "text/plain;charset=utf-8") == 0) {
        ssize_t written = write(fd, clipboard_text, sizeof(clipboard_text) - 1);
        if (written == (ssize_t)(sizeof(clipboard_text) - 1)) {
            clipboard_source_sent = 1;
        }
    }
    close(fd);
}

static void data_source_cancelled(void *data, struct wl_data_source *source) {
    (void)data; (void)source;
}

static const struct wl_data_source_listener data_source_listener = {
    .target = data_source_target,
    .send = data_source_send,
    .cancelled = data_source_cancelled,
};

static void data_offer_offer(void *data, struct wl_data_offer *offer,
                             const char *mime_type) {
    (void)data; (void)offer;
    if (mime_type != NULL &&
        strcmp(mime_type, "text/plain;charset=utf-8") == 0) {
        clipboard_mime_ok = 1;
    }
}

static const struct wl_data_offer_listener data_offer_listener = {
    .offer = data_offer_offer,
};

static void data_device_data_offer(void *data, struct wl_data_device *device,
                                   struct wl_data_offer *offer) {
    (void)data; (void)device;
    wl_data_offer_add_listener(offer, &data_offer_listener, NULL);
    clipboard_offer = offer;
}

static void data_device_selection(void *data, struct wl_data_device *device,
                                  struct wl_data_offer *offer) {
    (void)data; (void)device;
    if (offer != NULL) {
        clipboard_offer = offer;
        clipboard_selection = 1;
    }
}

static void data_device_enter(void *data, struct wl_data_device *device,
                              uint32_t serial, struct wl_surface *surface,
                              wl_fixed_t x, wl_fixed_t y,
                              struct wl_data_offer *offer) {
    (void)data; (void)device; (void)serial; (void)surface; (void)x; (void)y;
    (void)offer;
}

static void data_device_leave(void *data, struct wl_data_device *device) {
    (void)data; (void)device;
}

static void data_device_motion(void *data, struct wl_data_device *device,
                               uint32_t time, wl_fixed_t x, wl_fixed_t y) {
    (void)data; (void)device; (void)time; (void)x; (void)y;
}

static void data_device_drop(void *data, struct wl_data_device *device) {
    (void)data; (void)device;
}

static const struct wl_data_device_listener data_device_listener = {
    .data_offer = data_device_data_offer,
    .enter = data_device_enter,
    .leave = data_device_leave,
    .motion = data_device_motion,
    .drop = data_device_drop,
    .selection = data_device_selection,
};

static void decoration_configure(
    void *data, struct zxdg_toplevel_decoration_v1 *decoration,
    uint32_t mode) {
    (void)data; (void)decoration;
    decoration_configures += 1;
    if (mode == ZXDG_TOPLEVEL_DECORATION_V1_MODE_CLIENT_SIDE) {
        decoration_client_side = 1;
    }
}

static const struct zxdg_toplevel_decoration_v1_listener decoration_listener = {
    .configure = decoration_configure,
};

static void xdg_output_position(void *data, struct zxdg_output_v1 *output,
                                  int32_t x, int32_t y) {
    (void)data; (void)output;
    if (x != 0 || y != 0) {
        xdg_output_ok = 0;
    }
}

static void xdg_output_size(void *data, struct zxdg_output_v1 *output,
                            int32_t width, int32_t height) {
    (void)data; (void)output;
    if (width == 1024 && height == 768) {
        xdg_output_ok = 1;
    }
}

static void xdg_output_name(void *data, struct zxdg_output_v1 *output,
                            const char *name) {
    (void)data; (void)output;
    if (name == NULL || strcmp(name, "Werewolf panel") != 0) {
        xdg_output_ok = 0;
    }
}

static void xdg_output_description(void *data, struct zxdg_output_v1 *output,
                                   const char *description) {
    (void)data; (void)output;
    if (description == NULL ||
        strcmp(description, "WerewolfOS 1024x768 panel") != 0) {
        xdg_output_ok = 0;
    }
}

static void xdg_output_done(void *data, struct zxdg_output_v1 *output) {
    (void)data; (void)output;
}

static const struct zxdg_output_v1_listener xdg_output_listener = {
    .logical_position = xdg_output_position,
    .logical_size = xdg_output_size,
    .done = xdg_output_done,
    .name = xdg_output_name,
    .description = xdg_output_description,
};

static void activation_token_done(void *data,
                                  struct xdg_activation_token_v1 *token,
                                  const char *value) {
    (void)data; (void)token;
    if (value != NULL && strcmp(value, "werewolf-token") == 0) {
        activation_token_ok = 1;
    }
}

static const struct xdg_activation_token_v1_listener activation_listener = {
    .done = activation_token_done,
};

static void fractional_preferred_scale(void *data,
                                       struct wp_fractional_scale_v1 *scale,
                                       uint32_t preferred) {
    (void)data; (void)scale;
    if (preferred == 240) {
        fractional_ok = 1;
    }
}

static const struct wp_fractional_scale_v1_listener fractional_listener = {
    .preferred_scale = fractional_preferred_scale,
};

static void presentation_clock(void *data, struct wp_presentation *presentation,
                               uint32_t clock_id) {
    (void)data; (void)presentation;
    if (clock_id == 1) {
        presentation_clock_ok = 1;
    }
}

static const struct wp_presentation_listener presentation_listener = {
    .clock_id = presentation_clock,
};

static void feedback_sync_output(void *data,
                                 struct wp_presentation_feedback *feedback,
                                 struct wl_output *output) {
    (void)data; (void)feedback; (void)output;
}

static void feedback_presented(void *data,
                               struct wp_presentation_feedback *feedback,
                               uint32_t tv_sec_hi, uint32_t tv_sec_lo,
                               uint32_t tv_nsec, uint32_t refresh,
                               uint32_t seq_hi, uint32_t seq_lo,
                               uint32_t flags) {
    (void)data; (void)feedback; (void)tv_sec_hi; (void)tv_sec_lo;
    (void)tv_nsec; (void)seq_hi; (void)seq_lo;
    if (refresh == 16666666 && (flags & 1) != 0) {
        presentation_ok = 1;
    }
}

static void feedback_discarded(void *data,
                               struct wp_presentation_feedback *feedback) {
    (void)data; (void)feedback;
}

static const struct wp_presentation_feedback_listener feedback_listener = {
    .sync_output = feedback_sync_output,
    .presented = feedback_presented,
    .discarded = feedback_discarded,
};

static void relative_motion(void *data, struct zwp_relative_pointer_v1 *pointer,
                             uint32_t utime_hi, uint32_t utime_lo,
                             wl_fixed_t dx, wl_fixed_t dy,
                             wl_fixed_t dx_unaccel, wl_fixed_t dy_unaccel) {
    (void)data; (void)pointer; (void)utime_hi; (void)utime_lo;
    (void)dx_unaccel; (void)dy_unaccel;
    if (wl_fixed_to_int(dx) == 3 && wl_fixed_to_int(dy) == 4) {
        relative_ok = 1;
    }
}

static const struct zwp_relative_pointer_v1_listener relative_listener = {
    .relative_motion = relative_motion,
};

static void pointer_locked(void *data, struct zwp_locked_pointer_v1 *pointer) {
    (void)data; (void)pointer;
    locked_ok = 1;
}

static void pointer_unlocked(void *data, struct zwp_locked_pointer_v1 *pointer) {
    (void)data; (void)pointer;
}

static const struct zwp_locked_pointer_v1_listener locked_listener = {
    .locked = pointer_locked,
    .unlocked = pointer_unlocked,
};

static void text_input_enter(void *data, struct zwp_text_input_v3 *input,
                             struct wl_surface *surface) {
    (void)data; (void)input; (void)surface;
    text_input_entered = 1;
}

static void text_input_leave(void *data, struct zwp_text_input_v3 *input,
                             struct wl_surface *surface) {
    (void)data; (void)input; (void)surface;
}

static void text_input_preedit(void *data, struct zwp_text_input_v3 *input,
                               const char *text, int32_t begin,
                               int32_t end) {
    (void)data; (void)input; (void)text; (void)begin; (void)end;
}

static void text_input_commit(void *data, struct zwp_text_input_v3 *input,
                              const char *text) {
    (void)data; (void)input;
    if (text != NULL && strcmp(text, "werewolf") == 0) {
        text_input_committed = 1;
    }
}

static void text_input_delete(void *data, struct zwp_text_input_v3 *input,
                              uint32_t before, uint32_t after) {
    (void)data; (void)input; (void)before; (void)after;
}

static void text_input_done_event(void *data, struct zwp_text_input_v3 *input,
                                  uint32_t serial) {
    (void)data; (void)input;
    if (serial >= 1) {
        text_input_done = 1;
    }
}

static const struct zwp_text_input_v3_listener text_input_listener = {
    .enter = text_input_enter,
    .leave = text_input_leave,
    .preedit_string = text_input_preedit,
    .commit_string = text_input_commit,
    .delete_surrounding_text = text_input_delete,
    .done = text_input_done_event,
};

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
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        shell = wl_registry_bind(registry, name, &xdg_wm_base_interface, use);
    } else if (strcmp(interface, wl_output_interface.name) == 0) {
        uint32_t use = version < 4 ? version : 4;
        output = wl_registry_bind(registry, name, &wl_output_interface, use);
        wl_output_add_listener(output, &output_listener, NULL);
    } else if (strcmp(interface, wl_seat_interface.name) == 0) {
        uint32_t use = version < 5 ? version : 5;
        seat = wl_registry_bind(registry, name, &wl_seat_interface, use);
        wl_seat_add_listener(seat, &seat_listener, NULL);
    } else if (strcmp(interface, wl_data_device_manager_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        data_manager = wl_registry_bind(registry, name,
                                        &wl_data_device_manager_interface, use);
    } else if (strcmp(interface,
                      zxdg_decoration_manager_v1_interface.name) == 0) {
        uint32_t use = version < 2 ? version : 2;
        decoration_manager = wl_registry_bind(
            registry, name, &zxdg_decoration_manager_v1_interface, use);
    } else if (strcmp(interface, wp_viewporter_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        viewporter = wl_registry_bind(registry, name,
                                      &wp_viewporter_interface, use);
    } else if (strcmp(interface, wl_subcompositor_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        subcompositor = wl_registry_bind(registry, name,
                                         &wl_subcompositor_interface, use);
    } else if (strcmp(interface,
                      zxdg_output_manager_v1_interface.name) == 0) {
        uint32_t use = version < 3 ? version : 3;
        xdg_output_manager = wl_registry_bind(
            registry, name, &zxdg_output_manager_v1_interface, use);
    } else if (strcmp(interface, xdg_activation_v1_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        activation = wl_registry_bind(registry, name,
                                      &xdg_activation_v1_interface, use);
    } else if (strcmp(interface,
                      wp_fractional_scale_manager_v1_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        fractional_manager = wl_registry_bind(
            registry, name, &wp_fractional_scale_manager_v1_interface, use);
    } else if (strcmp(interface, wp_presentation_interface.name) == 0) {
        uint32_t use = version < 3 ? version : 3;
        presentation = wl_registry_bind(registry, name,
                                        &wp_presentation_interface, use);
        wp_presentation_add_listener(presentation, &presentation_listener, NULL);
    } else if (strcmp(interface,
                      zwp_relative_pointer_manager_v1_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        relative_manager = wl_registry_bind(
            registry, name, &zwp_relative_pointer_manager_v1_interface, use);
    } else if (strcmp(interface,
                      zwp_pointer_constraints_v1_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        constraints = wl_registry_bind(
            registry, name, &zwp_pointer_constraints_v1_interface, use);
    } else if (strcmp(interface,
                      zwp_text_input_manager_v3_interface.name) == 0) {
        uint32_t use = version < 1 ? version : 1;
        text_input_manager = wl_registry_bind(
            registry, name, &zwp_text_input_manager_v3_interface, use);
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

static void output_geometry(void *data, struct wl_output *out, int32_t x,
                            int32_t y, int32_t physical_width,
                            int32_t physical_height, int32_t subpixel,
                            const char *make, const char *model,
                            int32_t transform) {
    (void)data; (void)out; (void)x; (void)y; (void)physical_width;
    (void)physical_height; (void)subpixel; (void)make; (void)model;
    (void)transform;
}

static void output_mode(void *data, struct wl_output *out, uint32_t flags,
                        int32_t width, int32_t height, int32_t refresh) {
    (void)data; (void)out; (void)flags; (void)width; (void)height; (void)refresh;
}

static void output_done_event(void *data, struct wl_output *out) {
    (void)data; (void)out;
    output_done = 1;
}

static void output_scale_event(void *data, struct wl_output *out,
                               int32_t factor) {
    (void)data; (void)out;
    output_scale = factor;
}

static void output_name_event(void *data, struct wl_output *out,
                              const char *name) {
    (void)data; (void)out;
    output_name_ok = name != NULL && strcmp(name, "Werewolf panel") == 0;
}

static void output_description_event(void *data, struct wl_output *out,
                                     const char *description) {
    (void)data; (void)out;
    output_description_ok = description != NULL &&
                            strcmp(description,
                                   "WerewolfOS 1024x768 panel") == 0;
}

static void keyboard_keymap(void *data, struct wl_keyboard *kb,
                            uint32_t format, int32_t fd, uint32_t size) {
    (void)data; (void)kb; (void)format;
    char buffer[128] = {0};
    uint32_t want = size < sizeof(buffer) - 1 ? size : sizeof(buffer) - 1;
    /* SCM_RIGHTS shares the open file description, so the writer's offset
     * is ours too; rewind before reading the keymap. */
    lseek(fd, 0, SEEK_SET);
    ssize_t got = read(fd, buffer, want);
    close(fd);
    if (got > 0 && strstr(buffer, "WEREWOLF_KEYMAP") != NULL) {
        keymap_ok = 1;
    }
}

static void keyboard_enter(void *data, struct wl_keyboard *kb, uint32_t serial,
                           struct wl_surface *surface, struct wl_array *keys) {
    (void)data; (void)kb; (void)serial; (void)surface; (void)keys;
    key_enter = 1;
}

static void keyboard_leave(void *data, struct wl_keyboard *kb, uint32_t serial,
                           struct wl_surface *surface) {
    (void)data; (void)kb; (void)serial; (void)surface;
    key_left = 1;
}

static void keyboard_key(void *data, struct wl_keyboard *kb, uint32_t serial,
                         uint32_t time, uint32_t key, uint32_t state) {
    (void)data; (void)kb; (void)serial; (void)time; (void)key; (void)state;
}

static void keyboard_modifiers(void *data, struct wl_keyboard *kb,
                               uint32_t serial, uint32_t depressed,
                               uint32_t latched, uint32_t locked,
                               uint32_t group) {
    (void)data; (void)kb; (void)serial; (void)depressed; (void)latched;
    (void)locked; (void)group;
}

static void keyboard_repeat_info(void *data, struct wl_keyboard *kb,
                                 int32_t rate, int32_t delay);

static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keyboard_keymap,
    .enter = keyboard_enter,
    .leave = keyboard_leave,
    .key = keyboard_key,
    .modifiers = keyboard_modifiers,
    .repeat_info = keyboard_repeat_info,
};

static void pointer_enter(void *data, struct wl_pointer *ptr, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t sx,
                          wl_fixed_t sy) {
    (void)data; (void)ptr; (void)serial; (void)surface;
    if (wl_fixed_to_int(sx) == 100 && wl_fixed_to_int(sy) == 50) {
        pointer_ok = 1;
    }
}

static void pointer_leave(void *data, struct wl_pointer *ptr, uint32_t serial,
                          struct wl_surface *surface) {
    (void)data; (void)ptr; (void)serial; (void)surface;
    pointer_left = 1;
}

static void pointer_motion(void *data, struct wl_pointer *ptr, uint32_t time,
                           wl_fixed_t sx, wl_fixed_t sy) {
    (void)data; (void)ptr; (void)time; (void)sx; (void)sy;
}

static void pointer_button(void *data, struct wl_pointer *ptr, uint32_t serial,
                           uint32_t time, uint32_t button, uint32_t state) {
    (void)data; (void)ptr; (void)serial; (void)time; (void)button; (void)state;
}

static void pointer_axis(void *data, struct wl_pointer *ptr, uint32_t time,
                         uint32_t axis, wl_fixed_t value) {
    (void)data; (void)ptr; (void)time; (void)axis; (void)value;
}

static void frame_done_cb(void *data, struct wl_callback *callback,
                          uint32_t time) {
    (void)data; (void)time;
    frame_done = 1;
    wl_callback_destroy(callback);
}

static const struct wl_callback_listener frame_listener = {
    .done = frame_done_cb,
};

static void buffer_release(void *data, struct wl_buffer *buffer) {
    (void)data; (void)buffer;
    buffer_releases += 1;
}

static const struct wl_buffer_listener buffer_listener = {
    .release = buffer_release,
};

static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter,
    .leave = pointer_leave,
    .motion = pointer_motion,
    .button = pointer_button,
    .axis = pointer_axis,
};

static void seat_capabilities(void *data, struct wl_seat *s, uint32_t caps) {
    (void)data;
    (void)s;
    seat_caps = caps;
}

static void seat_name_event(void *data, struct wl_seat *s, const char *name) {
    (void)data; (void)s;
    seat_name_ok = name != NULL && strcmp(name, "Werewolf seat") == 0;
}

static void keyboard_repeat_info(void *data, struct wl_keyboard *kb,
                                 int32_t rate, int32_t delay) {
    (void)data; (void)kb;
    keyboard_repeat_ok = rate == 25 && delay == 600;
}

static void shell_ping(void *data, struct xdg_wm_base *base, uint32_t serial) {
    (void)data;
    xdg_wm_base_pong(base, serial);
}

static const struct xdg_wm_base_listener shell_listener = {
    .ping = shell_ping,
};

static void surface_configure(void *data, struct xdg_surface *surface,
                              uint32_t serial) {
    (void)data;
    (void)surface;
    configure_serial = serial;
    configured = 1;
}

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = surface_configure,
};

static void surface_enter_event(void *data, struct wl_surface *surface,
                                struct wl_output *output) {
    (void)data; (void)surface; (void)output;
    surface_entered = 1;
}

static void surface_leave_event(void *data, struct wl_surface *surface,
                                struct wl_output *output) {
    (void)data; (void)surface; (void)output;
}

static const struct wl_surface_listener wl_surface_listener = {
    .enter = surface_enter_event,
    .leave = surface_leave_event,
};

static void popup_surface_configure(void *data, struct xdg_surface *surface,
                                    uint32_t serial) {
    (void)data;
    (void)surface;
    configure_serial = serial;
    popup_configured = 1;
}

static const struct xdg_surface_listener popup_xdg_surface_listener = {
    .configure = popup_surface_configure,
};

static void popup_configure_event(void *data, struct xdg_popup *popup,
                                  int32_t x, int32_t y, int32_t width,
                                  int32_t height) {
    (void)data;
    (void)popup;
    popup_configure_ok = x == 40 && y == 30 && width == 200 && height == 120;
}

static void popup_done_event(void *data, struct xdg_popup *popup) {
    (void)data;
    (void)popup;
    popup_done = 1;
}

static const struct xdg_popup_listener popup_listener = {
    .configure = popup_configure_event,
    .popup_done = popup_done_event,
};

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel,
                               int32_t width, int32_t height,
                               struct wl_array *states) {
    (void)data;
    (void)toplevel;
    (void)width;
    (void)height;
    toplevel_maximized = 0;
    toplevel_fullscreen = 0;
    uint32_t *state;
    wl_array_for_each(state, states) {
        if (*state == XDG_TOPLEVEL_STATE_MAXIMIZED) {
            toplevel_maximized = 1;
        } else if (*state == XDG_TOPLEVEL_STATE_FULLSCREEN) {
            toplevel_fullscreen = 1;
        }
    }
}

static void toplevel_close(void *data, struct xdg_toplevel *toplevel) {
    (void)data;
    (void)toplevel;
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
};

int main(int argc, char **argv) {
    int is_source = argc > 1 && strcmp(argv[1], "source") == 0;
    int is_receiver = argc > 1 && strcmp(argv[1], "receiver") == 0;
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
    if (compositor == NULL || shared_memory == NULL || shell == NULL) {
        fprintf(stderr, "wlclient: missing globals\n");
        return 5;
    }
    xdg_wm_base_add_listener(shell, &shell_listener, NULL);
    wl_shm_add_listener(shared_memory, &shm_listener, NULL);
    if (wl_display_roundtrip(display) < 0) {
        return 6;
    }
    if (seat == NULL || (seat_caps & WL_SEAT_CAPABILITY_KEYBOARD) == 0 ||
        (seat_caps & WL_SEAT_CAPABILITY_POINTER) == 0) {
        fprintf(stderr, "wlclient: seat capabilities missing\n");
        return 6;
    }

    if (xdg_output_manager == NULL || output == NULL) {
        fprintf(stderr, "wlclient: xdg-output missing\n");
        return 6;
    }
    xdg_output = zxdg_output_manager_v1_get_xdg_output(xdg_output_manager,
                                                       output);
    if (xdg_output == NULL) {
        return 6;
    }
    zxdg_output_v1_add_listener(xdg_output, &xdg_output_listener, NULL);
    for (int attempt = 0; attempt < 4 && !xdg_output_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 6;
        }
    }
    if (!xdg_output_ok) {
        fprintf(stderr, "wlclient: xdg-output events missing\n");
        return 6;
    }

    data_device = wl_data_device_manager_get_data_device(data_manager, seat);
    if (data_device == NULL) {
        fprintf(stderr, "wlclient: data device missing\n");
        return 6;
    }
    wl_data_device_add_listener(data_device, &data_device_listener, NULL);
    if (!is_receiver) {
        data_source = wl_data_device_manager_create_data_source(data_manager);
        if (data_source == NULL) {
            fprintf(stderr, "wlclient: data source missing\n");
            return 6;
        }
        wl_data_source_add_listener(data_source, &data_source_listener, NULL);
        wl_data_source_offer(data_source, "text/plain;charset=utf-8");
        wl_data_device_set_selection(data_device, data_source, configure_serial);
    }

    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    if (surface == NULL) {
        return 7;
    }
    wl_surface_add_listener(surface, &wl_surface_listener, NULL);
    struct xdg_surface *xdg_surface =
        xdg_wm_base_get_xdg_surface(shell, surface);
    if (xdg_surface == NULL) {
        return 7;
    }
    xdg_surface_add_listener(xdg_surface, &xdg_surface_listener, NULL);
    struct xdg_toplevel *toplevel = xdg_surface_get_toplevel(xdg_surface);
    if (toplevel == NULL) {
        return 7;
    }
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    if (decoration_manager == NULL) {
        fprintf(stderr, "wlclient: decoration manager missing\n");
        return 7;
    }
    decoration = zxdg_decoration_manager_v1_get_toplevel_decoration(
        decoration_manager, toplevel);
    if (decoration == NULL) {
        return 7;
    }
    zxdg_toplevel_decoration_v1_add_listener(decoration, &decoration_listener,
                                             NULL);
    xdg_toplevel_set_title(toplevel, "werewolf-client");
    xdg_toplevel_set_app_id(toplevel, "org.werewolf.test");
    /* A bufferless commit asks the compositor for an initial configure. */
    wl_surface_commit(surface);
    while (!configured) {
        if (wl_display_roundtrip(display) < 0) {
            return 7;
        }
    }
    xdg_surface_ack_configure(xdg_surface, configure_serial);

    if (activation == NULL) {
        fprintf(stderr, "wlclient: activation missing\n");
        return 8;
    }
    activation_token = xdg_activation_v1_get_activation_token(activation);
    if (activation_token == NULL) {
        return 8;
    }
    xdg_activation_token_v1_add_listener(activation_token,
                                         &activation_listener, NULL);
    xdg_activation_token_v1_set_app_id(activation_token, "org.werewolf.test");
    xdg_activation_token_v1_set_surface(activation_token, surface);
    xdg_activation_token_v1_commit(activation_token);
    for (int attempt = 0; attempt < 4 && !activation_token_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!activation_token_ok) {
        fprintf(stderr, "wlclient: activation token missing\n");
        return 8;
    }
    if (fractional_manager == NULL) {
        fprintf(stderr, "wlclient: fractional scale missing\n");
        return 8;
    }
    fractional_scale = wp_fractional_scale_manager_v1_get_fractional_scale(
        fractional_manager, surface);
    if (fractional_scale == NULL) {
        return 8;
    }
    wp_fractional_scale_v1_add_listener(fractional_scale,
                                        &fractional_listener, NULL);
    for (int attempt = 0; attempt < 4 && !fractional_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!fractional_ok) {
        fprintf(stderr, "wlclient: fractional scale missing\n");
        return 8;
    }
    wp_fractional_scale_v1_destroy(fractional_scale);
    wp_fractional_scale_manager_v1_destroy(fractional_manager);
    if (presentation == NULL || !presentation_clock_ok) {
        fprintf(stderr, "wlclient: presentation clock missing\n");
        return 8;
    }
    presentation_feedback = wp_presentation_feedback(presentation, surface);
    if (presentation_feedback == NULL) {
        return 8;
    }
    wp_presentation_feedback_add_listener(presentation_feedback,
                                          &feedback_listener, NULL);
    for (int attempt = 0; attempt < 4 && !presentation_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!presentation_ok) {
        fprintf(stderr, "wlclient: presentation feedback missing\n");
        return 8;
    }
    wp_presentation_destroy(presentation);

    xdg_activation_v1_activate(activation, "werewolf-token", surface);
    xdg_activation_token_v1_destroy(activation_token);
    xdg_activation_v1_destroy(activation);

    for (int attempt = 0; attempt < 4 && !decoration_client_side; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!decoration_client_side || decoration_configures < 1) {
        fprintf(stderr, "wlclient: decoration configure missing\n");
        return 8;
    }
    zxdg_toplevel_decoration_v1_set_mode(
        decoration, ZXDG_TOPLEVEL_DECORATION_V1_MODE_CLIENT_SIDE);
    for (int attempt = 0; attempt < 4 && decoration_configures < 2; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    zxdg_toplevel_decoration_v1_unset_mode(decoration);
    for (int attempt = 0; attempt < 4 && decoration_configures < 3; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (decoration_configures < 3) {
        fprintf(stderr, "wlclient: decoration mode loop failed\n");
        return 8;
    }

    if (viewporter == NULL) {
        fprintf(stderr, "wlclient: viewporter missing\n");
        return 8;
    }
    viewport = wp_viewporter_get_viewport(viewporter, surface);
    if (viewport == NULL) {
        return 8;
    }
    wp_viewport_set_source(viewport, wl_fixed_from_int(0),
                           wl_fixed_from_int(0), wl_fixed_from_int(8),
                           wl_fixed_from_int(8));
    wp_viewport_set_destination(viewport, 16, 16);

    /* Toplevel state transitions: configure with MAXIMIZED, then empty, then
     * FULLSCREEN, then empty. */
    xdg_toplevel_set_maximized(toplevel);
    for (int attempt = 0; attempt < 4 && !toplevel_maximized; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!toplevel_maximized) {
        fprintf(stderr, "wlclient: maximized configure missing\n");
        return 8;
    }
    xdg_surface_ack_configure(xdg_surface, configure_serial);
    xdg_toplevel_unset_maximized(toplevel);
    for (int attempt = 0; attempt < 4 && toplevel_maximized; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (toplevel_maximized) {
        fprintf(stderr, "wlclient: unmaximized configure missing\n");
        return 8;
    }
    xdg_surface_ack_configure(xdg_surface, configure_serial);
    xdg_toplevel_set_fullscreen(toplevel, NULL);
    for (int attempt = 0; attempt < 4 && !toplevel_fullscreen; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!toplevel_fullscreen) {
        fprintf(stderr, "wlclient: fullscreen configure missing\n");
        return 8;
    }
    xdg_surface_ack_configure(xdg_surface, configure_serial);
    xdg_toplevel_unset_fullscreen(toplevel);
    for (int attempt = 0; attempt < 4 && toplevel_fullscreen; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (toplevel_fullscreen) {
        fprintf(stderr, "wlclient: unfullscreen configure missing\n");
        return 8;
    }
    xdg_surface_ack_configure(xdg_surface, configure_serial);

    /* Ask for input now that the surface exists, then receive keymap/enter. */
    keyboard = wl_seat_get_keyboard(seat);
    wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL);
    pointer = wl_seat_get_pointer(seat);
    wl_pointer_add_listener(pointer, &pointer_listener, NULL);
    if (wl_display_roundtrip(display) < 0) {
        return 7;
    }

    if (relative_manager == NULL) {
        fprintf(stderr, "wlclient: relative pointer missing\n");
        return 8;
    }
    relative_pointer = zwp_relative_pointer_manager_v1_get_relative_pointer(
        relative_manager, pointer);
    if (relative_pointer == NULL) {
        return 8;
    }
    zwp_relative_pointer_v1_add_listener(relative_pointer, &relative_listener,
                                         NULL);
    for (int attempt = 0; attempt < 4 && !relative_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!relative_ok) {
        fprintf(stderr, "wlclient: relative motion missing\n");
        return 8;
    }
    zwp_relative_pointer_v1_destroy(relative_pointer);
    zwp_relative_pointer_manager_v1_destroy(relative_manager);

    if (constraints == NULL) {
        fprintf(stderr, "wlclient: pointer constraints missing\n");
        return 8;
    }
    locked_pointer = zwp_pointer_constraints_v1_lock_pointer(
        constraints, surface, pointer, NULL,
        ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT);
    if (locked_pointer == NULL) {
        return 8;
    }
    zwp_locked_pointer_v1_add_listener(locked_pointer, &locked_listener, NULL);
    for (int attempt = 0; attempt < 4 && !locked_ok; attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!locked_ok) {
        fprintf(stderr, "wlclient: pointer lock missing\n");
        return 8;
    }
    zwp_locked_pointer_v1_destroy(locked_pointer);
    zwp_pointer_constraints_v1_destroy(constraints);

    if (text_input_manager == NULL) {
        fprintf(stderr, "wlclient: text input missing\n");
        return 8;
    }
    text_input = zwp_text_input_manager_v3_get_text_input(text_input_manager,
                                                          seat);
    if (text_input == NULL) {
        return 8;
    }
    zwp_text_input_v3_add_listener(text_input, &text_input_listener, NULL);
    zwp_text_input_v3_enable(text_input);
    zwp_text_input_v3_set_content_type(text_input, 0, 0);
    zwp_text_input_v3_set_cursor_rectangle(text_input, 0, 0, 8, 8);
    zwp_text_input_v3_commit(text_input);
    for (int attempt = 0;
         attempt < 6 &&
         !(text_input_entered && text_input_committed && text_input_done);
         attempt++) {
        if (wl_display_roundtrip(display) < 0) {
            return 8;
        }
    }
    if (!text_input_entered || !text_input_committed || !text_input_done) {
        fprintf(stderr, "wlclient: text input events missing\n");
        return 8;
    }
    zwp_text_input_v3_destroy(text_input);
    zwp_text_input_manager_v3_destroy(text_input_manager);

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

    struct wl_shm_pool *pool = wl_shm_create_pool(shared_memory, fd, 4096);
    struct wl_buffer *first = wl_shm_pool_create_buffer(
        pool, 0, width, height, stride, WL_SHM_FORMAT_ARGB8888);
    struct wl_buffer *second = wl_shm_pool_create_buffer(
        pool, 2048, width, height, stride, WL_SHM_FORMAT_ARGB8888);
    if (pool == NULL || first == NULL || second == NULL) {
        return 10;
    }
    wl_buffer_add_listener(first, &buffer_listener, NULL);
    wl_buffer_add_listener(second, &buffer_listener, NULL);
    pixels[0] = 0xFF112233u;
    pixels[1] = 0xFF445566u;
    uint32_t *second_pixels = (uint32_t *)((char *)pixels + 2048);
    second_pixels[0] = 0xFF778899u;
    second_pixels[1] = 0xFFAABBCCu;

    /* xdg_popup: positioner, popup configure, map, grab and popup_done. */
    struct xdg_positioner *positioner = xdg_wm_base_create_positioner(shell);
    if (positioner == NULL) {
        return 10;
    }
    xdg_positioner_set_size(positioner, 200, 120);
    xdg_positioner_set_anchor_rect(positioner, 0, 0, 1, 1);
    xdg_positioner_set_anchor(positioner, XDG_POSITIONER_ANCHOR_TOP_LEFT);
    xdg_positioner_set_gravity(positioner, XDG_POSITIONER_GRAVITY_BOTTOM_RIGHT);
    struct wl_surface *popup_surface =
        wl_compositor_create_surface(compositor);
    struct xdg_surface *popup_xdg_surface =
        xdg_wm_base_get_xdg_surface(shell, popup_surface);
    if (popup_surface == NULL || popup_xdg_surface == NULL) {
        return 10;
    }
    xdg_surface_add_listener(popup_xdg_surface, &popup_xdg_surface_listener,
                             NULL);
    struct xdg_popup *popup =
        xdg_surface_get_popup(popup_xdg_surface, xdg_surface, positioner);
    if (popup == NULL) {
        return 10;
    }
    xdg_popup_add_listener(popup, &popup_listener, NULL);
    while (!popup_configured) {
        if (wl_display_roundtrip(display) < 0) {
            return 10;
        }
    }
    if (!popup_configure_ok) {
        fprintf(stderr, "wlclient: popup configure wrong\n");
        return 10;
    }
    xdg_surface_ack_configure(popup_xdg_surface, configure_serial);
    wl_surface_attach(popup_surface, first, 0, 0);
    wl_surface_damage(popup_surface, 0, 0, 8, 8);
    wl_surface_commit(popup_surface);
    xdg_popup_grab(popup, seat, configure_serial);
    while (!popup_done) {
        if (wl_display_roundtrip(display) < 0) {
            return 10;
        }
    }
    xdg_popup_destroy(popup);
    xdg_surface_destroy(popup_xdg_surface);
    wl_surface_destroy(popup_surface);
    xdg_positioner_destroy(positioner);

    /* Clipboard: source offers text, receiver asks for it, source writes
     * through the passed fd. */
    if (is_source && !is_receiver) {
        for (int attempt = 0; attempt < 16 && !clipboard_source_sent; attempt++) {
            if (wl_display_roundtrip(display) < 0) {
                return 10;
            }
        }
        if (!clipboard_source_sent) {
            fprintf(stderr, "wlclient: clipboard source not drained\n");
            return 10;
        }
        printf("clipboard source ok\n");
    } else {
        for (int attempt = 0; attempt < 16 && !clipboard_selection; attempt++) {
            if (wl_display_roundtrip(display) < 0) {
                return 10;
            }
        }
        if (!clipboard_selection || !clipboard_mime_ok ||
            clipboard_offer == NULL) {
            fprintf(stderr, "wlclient: clipboard offer missing\n");
            return 10;
        }
        int clipboard_fds[2];
        if (pipe(clipboard_fds) != 0) {
            return 10;
        }
        wl_data_offer_receive(clipboard_offer, "text/plain;charset=utf-8",
                              clipboard_fds[1]);
        close(clipboard_fds[1]);
        /* The source is another process in the cross-client test, so just
         * block on the pipe while it writes; a roundtrip has already made the
         * server forward our receive request. */
        if (wl_display_roundtrip(display) < 0) {
            return 10;
        }
        char clipboard_buffer[64];
        ssize_t clipboard_length = read(clipboard_fds[0], clipboard_buffer,
                                        sizeof(clipboard_buffer));
        close(clipboard_fds[0]);
        if (clipboard_length != (ssize_t)(sizeof(clipboard_text) - 1) ||
            memcmp(clipboard_buffer, clipboard_text,
                   sizeof(clipboard_text) - 1) != 0) {
            fprintf(stderr, "wlclient: clipboard round-trip failed\n");
            return 10;
        }
        if (is_receiver) {
            printf("clipboard receiver ok\n");
        }
    }

    if (subcompositor == NULL) {
        fprintf(stderr, "wlclient: subcompositor missing\n");
        return 10;
    }
    struct wl_surface *child_surface =
        wl_compositor_create_surface(compositor);
    if (child_surface == NULL) {
        return 10;
    }
    subsurface = wl_subcompositor_get_subsurface(subcompositor, child_surface,
                                                 surface);
    if (subsurface == NULL) {
        return 10;
    }
    wl_subsurface_set_position(subsurface, 4, 4);
    wl_subsurface_set_sync(subsurface);
    wl_subsurface_set_desync(subsurface);
    wl_subsurface_place_above(subsurface, surface);
    wl_surface_attach(child_surface, second, 0, 0);
    wl_surface_damage(child_surface, 0, 0, 8, 8);
    wl_surface_commit(child_surface);
    if (wl_display_roundtrip(display) < 0) {
        return 10;
    }
    wl_subsurface_destroy(subsurface);
    wl_surface_destroy(child_surface);
    wl_subcompositor_destroy(subcompositor);

    /* Three frames alternating buffers, each waiting for frame + release. */
    for (int frame = 0; frame < 3; frame++) {
        struct wl_buffer *buffer = (frame & 1) != 0 ? second : first;
        struct wl_callback *callback = wl_surface_frame(surface);
        wl_callback_add_listener(callback, &frame_listener, NULL);
        frame_done = 0;
        wl_surface_attach(surface, buffer, 0, 0);
        wl_surface_damage(surface, 0, 0, width, height);
        wl_surface_commit(surface);
        while (!frame_done) {
            if (wl_display_roundtrip(display) < 0) {
                return 11;
            }
        }
    }
    if (buffer_releases < 3) {
        fprintf(stderr, "wlclient: buffer releases missing\n");
        return 11;
    }
    if (!surface_entered || !output_done || output_scale != 2 ||
        !output_name_ok || !output_description_ok || !seat_name_ok ||
        !keyboard_repeat_ok || !keymap_ok || !pointer_ok || !key_enter) {
        fprintf(stderr,
                "wlclient: events missing surface=%d out=%d scale=%d "
                "oname=%d odesc=%d sname=%d krepeat=%d map=%d ptr=%d enter=%d\n",
                surface_entered, output_done, output_scale, output_name_ok,
                output_description_ok, seat_name_ok, keyboard_repeat_ok,
                keymap_ok, pointer_ok, key_enter);
        return 11;
    }

    /* Destroy the shell surface; the compositor must drop focus. */
    zxdg_output_v1_destroy(xdg_output);
    zxdg_output_manager_v1_destroy(xdg_output_manager);
    wp_viewport_destroy(viewport);
    wp_viewporter_destroy(viewporter);
    zxdg_toplevel_decoration_v1_destroy(decoration);
    zxdg_decoration_manager_v1_destroy(decoration_manager);
    xdg_toplevel_destroy(toplevel);
    xdg_surface_destroy(xdg_surface);
    wl_surface_destroy(surface);
    if (wl_display_roundtrip(display) < 0) {
        return 11;
    }
    if (!key_left || !pointer_left) {
        fprintf(stderr, "wlclient: focus leave missing key=%d pointer=%d\n",
                key_left, pointer_left);
        return 11;
    }

    printf("libwayland client ok\n");
    fflush(stdout);
    wl_display_disconnect(display);
    return 0;
}
