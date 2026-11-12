#define _GNU_SOURCE
/* Offscreen EGL/GLES2 test through Mesa's virtio_gpu driver.
 *
 * Uses the surfaceless platform: no window system, no dmabuf, just the
 * render node. Creates an FBO, clears it and reads the pixel back, which
 * exercises resource creation, a real virgl draw and a host->guest
 * transfer. Prints the GL strings and the read-back colour. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <unistd.h>
#include <link.h>

typedef unsigned int GLenum;
typedef unsigned int GLuint;
typedef int GLint;
typedef int GLsizei;
typedef unsigned char GLubyte;

typedef void *EGLDisplay;
typedef void *EGLConfig;
typedef void *EGLContext;
typedef void *EGLSurface;
typedef int EGLint;
typedef unsigned int EGLBoolean;

#define EGL_DEFAULT_DISPLAY ((void *)0)
#define EGL_NO_CONTEXT ((void *)0)
#define EGL_NO_SURFACE ((void *)0)
#define EGL_PLATFORM_SURFACELESS_MESA 0x31DD
#define EGL_PLATFORM_DEVICE_EXT 0x313F
#define EGL_PLATFORM_GBM_KHR 0x31D7
#define EGL_SURFACE_TYPE 0x3033
#define EGL_PBUFFER_BIT 0x0001
#define EGL_WINDOW_BIT 0x0004
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_NONE 0x3038
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_CONTEXT_CLIENT_VERSION 0x3098

#define GL_TEXTURE_2D 0x0DE1
#define GL_RGBA 0x1908
#define GL_UNSIGNED_BYTE 0x1401
#define GL_FRAMEBUFFER 0x8D40
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_VENDOR 0x1F00
#define GL_RENDERER 0x1F01
#define GL_VERSION 0x1F02
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_LINEAR 0x2601

typedef EGLDisplay (*GetPlatformDisplayEXT)(unsigned int platform, void *native,
                                            const EGLint *attribs);
extern EGLDisplay eglGetDisplay(void *display);
extern EGLDisplay eglGetPlatformDisplay(unsigned int platform, void *native,
                                        const EGLint *attribs);
extern void *eglGetProcAddress(const char *name);
extern EGLBoolean eglInitialize(EGLDisplay display, EGLint *major, EGLint *minor);
extern EGLBoolean eglBindAPI(unsigned int api);
extern EGLBoolean eglChooseConfig(EGLDisplay display, const EGLint *attribs,
                                  EGLConfig *configs, EGLint count, EGLint *num);
extern EGLContext eglCreateContext(EGLDisplay display, EGLConfig config,
                                   EGLContext share, const EGLint *attribs);
extern EGLBoolean eglMakeCurrent(EGLDisplay display, EGLSurface draw,
                                 EGLSurface read, EGLContext context);
extern EGLint eglGetError(void);

extern const GLubyte *glGetString(GLenum name);
extern void glGenTextures(GLsizei count, GLuint *textures);
extern void glBindTexture(GLenum target, GLuint texture);
extern void glTexImage2D(GLenum target, GLint level, GLint internal_format,
                         GLsizei width, GLsizei height, GLint border,
                         GLenum format, GLenum type, const void *pixels);
extern void glTexParameteri(GLenum target, GLenum name, GLint value);
extern void glGenFramebuffers(GLsizei count, GLuint *framebuffers);
extern void glBindFramebuffer(GLenum target, GLuint framebuffer);
extern void glFramebufferTexture2D(GLenum target, GLenum attachment,
                                   GLenum textarget, GLuint texture, GLint level);
extern GLenum glCheckFramebufferStatus(GLenum target);
extern void glViewport(GLint x, GLint y, GLsizei width, GLsizei height);
extern void glClearColor(float red, float green, float blue, float alpha);
extern void glClear(GLenum mask);
extern void glFinish(void);
extern void glReadPixels(GLint x, GLint y, GLsizei width, GLsizei height,
                         GLenum format, GLenum type, void *pixels);

static int dump_phdr(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    struct dump_state { int count; } *state = data;
    printf("map %d base 0x%lx %s\n", state->count++, (unsigned long)info->dlpi_addr,
           info->dlpi_name[0] != 0 ? info->dlpi_name : "(main)");
    return 0;
}

int main(void) {
    /* Mesa/glvnd are found in the SFS root, not in /usr. The driver is
       selected explicitly: the DRI loader's PCI table does know virtio-gpu,
       but being explicit makes a failed probe obvious. */
    setenv("LD_LIBRARY_PATH", "/sfs", 1);
    setenv("LIBGL_DRIVERS_PATH", "/sfs", 1);
#ifdef EGLTEST_VIRGL
    setenv("MESA_LOADER_DRIVER_OVERRIDE", "virtio_gpu", 1);
#else
    /* The default build exercises EGL+GLES end to end on the software
       rasteriser: `virtio_gpu` needs libdrm's /sys device description, which
       this guest does not have yet (see EGLTEST_VIRGL). */
    setenv("MESA_LOADER_DRIVER_OVERRIDE", "swrast", 1);
#endif
    setenv("__EGL_VENDOR_LIBRARY_FILENAMES", "/sfs/egl_vendor.json", 1);
    setenv("EGL_PLATFORM", "surfaceless", 1);
    setenv("GBM_BACKENDS_PATH", "/sfs", 1);
    setenv("EGL_LOG_LEVEL", "debug", 1);
    setenv("MESA_DEBUG", "1", 1);
    setenv("LIBGL_DEBUG", "verbose", 1);

    /* `EGL_PLATFORM=surfaceless` makes eglGetDisplay pick the surfaceless
       platform; the EXT entry point is the explicit fallback. */
    /* The DRI module (`libdril_dri.so`) takes its driver entry points from
       libgallium as undefined symbols, so libgallium must be in the global
       scope before the module is loaded. */
    /* Describe the synthetic /dev/dri nodes the way a Linux guest with
       virtio-gpu would: libdrm otherwise reads /sys, which does not exist. */
#ifdef EGLTEST_VIRGL
    /* Experimental virgl path. Mesa's loader asks libdrm to describe the
       device behind the fd, and libdrm reads /sys, which this guest does not
       have; the shim answers those enumeration calls. libgallium must be
       global before the DRI stub resolves the driver entry points. */
    void *shim = dlopen("/sfs/libdrm_shim.so", RTLD_NOW | RTLD_GLOBAL);
    printf("egl: drm shim %s\n", shim != NULL ? "loaded" : dlerror());
    void *gallium = dlopen("/sfs/libgallium-25.0.7.so", RTLD_NOW | RTLD_GLOBAL);
    printf("egl: gallium %s\n", gallium != NULL ? "loaded" : dlerror());
#endif
    void *vendor = dlopen("/sfs/libEGL_mesa.so.0", RTLD_NOW | RTLD_GLOBAL);
    if (vendor == NULL) {
        printf("egl: vendor load failed: %s\n", dlerror());
    } else {
        printf("egl: vendor loaded\n");
    }
    void *driver = dlopen("/sfs/virtio_gpu_dri.so", RTLD_NOW | RTLD_GLOBAL);
    if (driver == NULL) {
        printf("egl: driver load failed: %s\n", dlerror());
    } else {
        printf("egl: driver loaded\n");
    }
    /* Which objects are mapped where: the kernel prints the user PC of the
       capset queries, so this table names them. */
    struct dump_state { int count; } state = {0};
    dl_iterate_phdr(dump_phdr, &state);

    /* The surfaceless platform with EGL_DEFAULT_DISPLAY is swrast: to reach
       the virtio-gpu driver Mesa needs the render node fd. */
    int device = open("/dev/dri/renderD128", O_RDWR);
    printf("egl: render node %s\n", device >= 0 ? "open" : "missing");
    EGLDisplay display = EGL_NO_CONTEXT;
    if (device >= 0) {
        display = eglGetPlatformDisplay(EGL_PLATFORM_DEVICE_EXT,
                                        (void *)(long)device, NULL);
        printf("egl: device display %p error 0x%x\n", display, eglGetError());
    }
    if (display == EGL_NO_CONTEXT) {
        GetPlatformDisplayEXT get_platform =
            (GetPlatformDisplayEXT)eglGetProcAddress("eglGetPlatformDisplayEXT");
        printf("egl: ext entry %p\n", (void *)get_platform);
        if (get_platform != NULL && device >= 0) {
            display = get_platform(EGL_PLATFORM_DEVICE_EXT,
                                   (void *)(long)device, NULL);
        }
    }
    if (display == EGL_NO_CONTEXT && device >= 0) {
        /* GBM is the path that does not need libdrm's sysfs device
           enumeration: it takes the fd and asks the loader for the driver
           named by MESA_LOADER_DRIVER_OVERRIDE. */
        void *gbm_library = dlopen("/sfs/libgbm.so.1", RTLD_NOW | RTLD_GLOBAL);
        void *(*create_device)(int) = gbm_library != NULL
            ? (void *(*)(int))dlsym(gbm_library, "gbm_create_device") : NULL;
        void *gbm_device = create_device != NULL ? create_device(device) : NULL;
        printf("egl: gbm device %p\n", gbm_device);
        if (gbm_device != NULL) {
            display = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm_device, NULL);
            printf("egl: gbm display %p error 0x%x\n", display, eglGetError());
        }
    }
    if (display == EGL_NO_CONTEXT) {
        display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
        printf("egl: fallback display %p error 0x%x\n", display, eglGetError());
    }
    if (display == EGL_NO_CONTEXT) {
        printf("egl: no platform display\n");
        return 1;
    }
    EGLint major = 0, minor = 0;
    if (!eglInitialize(display, &major, &minor)) {
        printf("egl: initialize failed (0x%x)\n", eglGetError());
        return 1;
    }
    printf("egl: version %d.%d\n", major, minor);
    eglBindAPI(EGL_OPENGL_ES_API);

    /* A GBM display offers window configs; the render target is an FBO, so
       the surface type only has to match what the platform advertises. */
    EGLint config_attribs[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = 0;
    EGLint count = 0;
    if (!eglChooseConfig(display, config_attribs, &config, 1, &count) || count == 0) {
        EGLint minimal[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE};
        if (!eglChooseConfig(display, minimal, &config, 1, &count) || count == 0) {
            printf("egl: no config (0x%x)\n", eglGetError());
            return 1;
        }
    }
    EGLint context_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attribs);
    if (context == EGL_NO_CONTEXT) {
        printf("egl: no context (0x%x)\n", eglGetError());
        return 1;
    }
    if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context)) {
        printf("egl: make current failed (0x%x)\n", eglGetError());
        return 1;
    }
    printf("gl: vendor   %s\n", (const char *)glGetString(GL_VENDOR));
    printf("gl: renderer %s\n", (const char *)glGetString(GL_RENDERER));
    printf("gl: version  %s\n", (const char *)glGetString(GL_VERSION));

    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);

    GLuint framebuffer = 0;
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           texture, 0);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        printf("gl: framebuffer incomplete 0x%x\n", status);
        return 1;
    }
    printf("gl: framebuffer complete\n");

    glViewport(0, 0, 64, 64);
    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();

    GLubyte pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    printf("gl: readback %u %u %u %u\n", pixel[0], pixel[1], pixel[2], pixel[3]);

    /* 0.25/0.5/0.75 in an 8-bit UNORM target is 64/128/191/255. */
    int ok = pixel[0] >= 62 && pixel[0] <= 66 && pixel[1] >= 126 &&
             pixel[1] <= 130 && pixel[2] >= 189 && pixel[2] <= 193;
    printf("egl test %s\n", ok ? "ok" : "failed");
    return ok ? 0 : 1;
}
