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
#include <sys/stat.h>
#include <sys/sysmacros.h>

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

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    /* Mesa/glvnd live in the SFS root, not /usr. The driver is chosen
       explicitly so a failed probe is visible instead of a silent llvmpipe
       fallback. */
    setenv("LD_LIBRARY_PATH", "/sfs", 1);
    setenv("LIBGL_DRIVERS_PATH", "/sfs", 1);
    setenv("__EGL_VENDOR_LIBRARY_FILENAMES", "/sfs/egl_vendor.json", 1);
    setenv("EGL_PLATFORM", "surfaceless", 1);
    setenv("GBM_BACKENDS_PATH", "/sfs", 1);
#ifdef EGLTEST_SWRAST
    setenv("MESA_LOADER_DRIVER_OVERRIDE", "swrast", 1);
#else
    setenv("MESA_LOADER_DRIVER_OVERRIDE", "virtio_gpu", 1);
#endif
    setenv("EGL_LOG_LEVEL", "warning", 1);

#ifndef EGLTEST_SWRAST
    /* The DRI stub (`libdril_dri.so`) resolves its driver entry points from
       libgallium, so libgallium must be in the global scope first. */
    void *gallium = dlopen("/sfs/libgallium-25.0.7.so", RTLD_NOW | RTLD_GLOBAL);
    if (gallium == NULL) {
        printf("egl: gallium load failed: %s\n", dlerror());
        return 1;
    }
#endif
    void *vendor = dlopen("/sfs/libEGL_mesa.so.0", RTLD_NOW | RTLD_GLOBAL);
    if (vendor == NULL) {
        printf("egl: vendor load failed: %s\n", dlerror());
    } else {
        printf("egl: vendor loaded\n");
    }

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
