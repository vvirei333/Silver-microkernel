/* Real libinput 1.28 with its udev backend, against the kernel sysfs. */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <unistd.h>

struct udev;
struct libinput;
struct libinput_event;
struct libinput_device;

struct libinput_interface {
    int (*open_restricted)(const char *path, int flags, void *user_data);
    void (*close_restricted)(int fd, void *user_data);
};

extern struct udev *udev_new(void);
extern struct udev *udev_unref(struct udev *);
extern struct libinput *libinput_udev_create_context(
    const struct libinput_interface *, void *, struct udev *);
extern int libinput_udev_assign_seat(struct libinput *, const char *);
extern int libinput_get_fd(struct libinput *);
extern int libinput_dispatch(struct libinput *);
extern struct libinput_event *libinput_get_event(struct libinput *);
extern int libinput_event_get_type(struct libinput_event *);
extern struct libinput_device *libinput_event_get_device(struct libinput_event *);
extern void libinput_event_destroy(struct libinput_event *);
extern const char *libinput_device_get_name(struct libinput_device *);
extern int libinput_device_has_capability(struct libinput_device *, int);
extern struct libinput *libinput_unref(struct libinput *);

#define LIBINPUT_EVENT_DEVICE_ADDED 1
#define LIBINPUT_CAPABILITY_KEYBOARD 0
#define LIBINPUT_CAPABILITY_POINTER 1

static int open_restricted(const char *path, int flags, void *user)
{
    (void)user;
    int fd = open(path, flags | O_CLOEXEC);
    return fd < 0 ? -errno : fd;
}

static void close_restricted(int fd, void *user)
{
    (void)user;
    close(fd);
}

static const struct libinput_interface interface = {
    .open_restricted = open_restricted,
    .close_restricted = close_restricted,
};

int main(void)
{
    struct udev *udev = udev_new();
    if (udev == NULL) {
        printf("udev_new failed\n");
        return 1;
    }
    struct libinput *li = libinput_udev_create_context(&interface, NULL, udev);
    if (li == NULL) {
        printf("libinput_udev_create_context failed errno=%d\n", errno);
        udev_unref(udev);
        return 1;
    }
    int seat = libinput_udev_assign_seat(li, "seat0");
    int fd = libinput_get_fd(li);

    int added = 0;
    int keyboard = 0;
    int pointer = 0;
    for (int rounds = 0; rounds < 20 && added < 2; rounds++) {
        struct pollfd p = { .fd = fd, .events = POLLIN };
        int ready = poll(&p, 1, 1000);
        if (ready < 0 && errno != EINTR) {
            printf("poll failed errno=%d\n", errno);
            break;
        }
        if (libinput_dispatch(li) < 0) {
            printf("dispatch failed errno=%d\n", errno);
            break;
        }
        struct libinput_event *event;
        while ((event = libinput_get_event(li)) != NULL) {
            if (libinput_event_get_type(event) == LIBINPUT_EVENT_DEVICE_ADDED) {
                struct libinput_device *device = libinput_event_get_device(event);
                const char *name = libinput_device_get_name(device);
                int key = libinput_device_has_capability(device, LIBINPUT_CAPABILITY_KEYBOARD);
                int ptr = libinput_device_has_capability(device, LIBINPUT_CAPABILITY_POINTER);
                printf("device added name=%s keyboard=%d pointer=%d\n",
                       name ? name : "-", key, ptr);
                added++;
                keyboard |= key;
                pointer |= ptr;
            }
            libinput_event_destroy(event);
        }
    }
    printf("added=%d keyboard=%d pointer=%d\n", added, keyboard, pointer);
    int ok = seat == 0 && added >= 2 && keyboard && pointer;
    printf(ok ? "libinput ok\n" : "libinput FAILED\n");
    libinput_unref(li);
    udev_unref(udev);
    return ok ? 0 : 1;
}
