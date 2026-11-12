#define _GNU_SOURCE
/* Wayland EGL window-surface test.
 *
 * Unlike egltest (surfaceless + an application FBO), this creates a real
 * wl_egl_window, makes its surface current and draws to the *default*
 * framebuffer, exactly like a toolkit does. That path exercises Mesa's
 * Wayland platform swapchain, the compositor's dmabuf import and the host's
 * default-framebuffer handling.
 *
 * It prints `wlgl: ...` lines and ends with `wlgl test ok` or `wlgl failed`.
 *
 * Build (the guest rootfs needs the aarch64 EGL/Wayland libraries):
 *   aarch64-linux-gnu-gcc -O2 -I<root>/usr/include -o build/wlgltest \
 *       tests/linux/wlgltest.c <root>/usr/lib/libwayland-client.so.0 \
 *       <root>/usr/lib/libwayland-egl.so.1 <root>/usr/lib/libEGL.so.1 \
 *       <root>/usr/lib/libffi.so.8 <root>/usr/lib/libGLdispatch.so.0
 *   patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
 *       --set-rpath '$ORIGIN' build/wlgltest
 * Run it against the compositor with `linuxstart wlgl` (SERVE_WLGL).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
#include <wayland-egl.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <link.h>

static int print_object(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    (void)data;
    unsigned long end = 0;
    for (unsigned index = 0; index < info->dlpi_phnum; index++) {
        unsigned long candidate = info->dlpi_addr + info->dlpi_phdr[index].p_vaddr +
                                  info->dlpi_phdr[index].p_memsz;
        if (candidate > end) {
            end = candidate;
        }
    }
    printf("wlgl: object %s base 0x%lx end 0x%lx\n",
           info->dlpi_name != NULL && info->dlpi_name[0] != 0
               ? info->dlpi_name
               : "(main)",
           (unsigned long)info->dlpi_addr, end);
    return 0;
}

typedef unsigned int GLenum;
typedef unsigned int GLuint;
typedef void (*GL_CLEAR_COLOR)(float, float, float, float);
typedef void (*GL_CLEAR)(GLuint);
typedef GLenum (*GL_GET_ERROR)(void);
typedef const unsigned char *(*GL_GET_STRING)(GLenum);

#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_RENDERER 0x1F01
#define GL_VERSION 0x1F02
#define EGL_PLATFORM_WAYLAND_KHR 0x31D8
#define EGL_OPENGL_ES2_BIT 0x0004

typedef EGLDisplay (*PFN_GET_PLATFORM_DISPLAY)(EGLenum, void *, const EGLint *);

static struct wl_display *display;
static struct wl_compositor *compositor;

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    (void)data;
    (void)version;
    if (strcmp(interface, "wl_compositor") == 0) {
        compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                      4);
    }
}

static void registry_remove(void *data, struct wl_registry *registry,
                            uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    registry_global,
    registry_remove,
};

static void fail(const char *what) {
    printf("wlgl: %s\n", what);
    fflush(stdout);
    exit(1);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    display = wl_display_connect(NULL);
    if (display == NULL) {
        fail("no compositor");
    }
    printf("wlgl: display fd %d\n", wl_display_get_fd(display));
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    printf("wlgl: roundtrip done\n");
    if (compositor == NULL) {
        fail("no wl_compositor");
    }
    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    if (surface == NULL) {
        fail("no surface");
    }

    PFN_GET_PLATFORM_DISPLAY get_platform_display =
        (PFN_GET_PLATFORM_DISPLAY)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (get_platform_display == NULL) {
        fail("no eglGetPlatformDisplayEXT");
    }
    EGLDisplay egl_display =
        get_platform_display(EGL_PLATFORM_WAYLAND_KHR, display, NULL);
    EGLint major = 0, minor = 0;
    if (!eglInitialize(egl_display, &major, &minor)) {
        fail("eglInitialize failed");
    }
    printf("wlgl: EGL %d.%d\n", major, minor);

    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(egl_display, config_attributes, &config, 1,
                         &config_count) ||
        config_count == 0) {
        fail("eglChooseConfig found no window config");
    }
    printf("wlgl: config count %d\n", config_count);

    struct wl_egl_window *egl_window = wl_egl_window_create(surface, 256, 256);
    if (egl_window == NULL) {
        fail("wl_egl_window_create failed");
    }
    EGLSurface egl_surface =
        eglCreateWindowSurface(egl_display, config, egl_window, NULL);
    if (egl_surface == EGL_NO_SURFACE) {
        printf("wlgl: eglCreateWindowSurface error 0x%x\n", eglGetError());
        fail("eglCreateWindowSurface failed");
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    const EGLint context_attributes[] = {
        EGL_CONTEXT_CLIENT_VERSION, 2,
        EGL_NONE,
    };
    EGLContext context = eglCreateContext(egl_display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    if (context == EGL_NO_CONTEXT) {
        printf("wlgl: eglCreateContext error 0x%x\n", eglGetError());
        fail("eglCreateContext failed");
    }
    if (!eglMakeCurrent(egl_display, egl_surface, egl_surface, context)) {
        printf("wlgl: eglMakeCurrent error 0x%x\n", eglGetError());
        fail("eglMakeCurrent failed");
    }

    eglSwapInterval(egl_display, 0);
    GL_GET_STRING get_string = (GL_GET_STRING)eglGetProcAddress("glGetString");
    GL_CLEAR_COLOR clear_color =
        (GL_CLEAR_COLOR)eglGetProcAddress("glClearColor");
    GL_CLEAR clear = (GL_CLEAR)eglGetProcAddress("glClear");
    GL_GET_ERROR get_error = (GL_GET_ERROR)eglGetProcAddress("glGetError");
    if (get_string != NULL) {
        printf("wlgl: renderer %s\n", (const char *)get_string(GL_RENDERER));
    }
    if (clear_color == NULL || clear == NULL || get_error == NULL) {
        fail("no GL entry points");
    }
    dl_iterate_phdr(print_object, NULL);
    printf("wlgl: drawing to the window framebuffer\n");
    clear_color(1.0f, 0.25f, 0.0f, 1.0f);
    clear(GL_COLOR_BUFFER_BIT);
    GLenum error = get_error();
    printf("wlgl: gl error after clear 0x%x\n", error);
    if (!eglSwapBuffers(egl_display, egl_surface)) {
        printf("wlgl: eglSwapBuffers error 0x%x\n", eglGetError());
        fail("eglSwapBuffers failed");
    }
    error = get_error();
    printf("wlgl: gl error after swap 0x%x\n", error);

    /* A second frame with a different colour: the compositor maps the next
       buffer, which proves the host->guest copy happens per commit and not
       only for the first buffer. */
    printf("wlgl: second swap\n");
    wl_display_roundtrip(display);
    eglSwapInterval(egl_display, 0);
    clear_color(0.1f, 0.8f, 0.2f, 1.0f);
    clear(GL_COLOR_BUFFER_BIT);
    if (!eglSwapBuffers(egl_display, egl_surface)) {
        printf("wlgl: second eglSwapBuffers error 0x%x\n", eglGetError());
        fail("second eglSwapBuffers failed");
    }
    error = get_error();
    printf("wlgl: gl error after second swap 0x%x\n", error);
    printf("wlgl test ok\n");
    fflush(stdout);
    return 0;
}
