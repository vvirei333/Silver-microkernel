/* Real systemd libudev against the kernel's synthetic sysfs.
 *
 * libudev has no public header in the test toolchain, so the handful of
 * prototypes below are declared by hand. The test asks libudev to enumerate
 * the input subsystem, then resolves every syspath into a device and prints
 * the devnode and the udev properties libinput looks at.
 *
 * Success: event0 and event1 are enumerated, each has /dev/input/eventN and
 * ID_INPUT=1.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>

struct udev;
struct udev_enumerate;
struct udev_list_entry;
struct udev_device;

extern struct udev *udev_new(void);
extern struct udev *udev_unref(struct udev *);
extern struct udev_enumerate *udev_enumerate_new(struct udev *);
extern struct udev_enumerate *udev_enumerate_unref(struct udev_enumerate *);
extern int udev_enumerate_add_match_subsystem(struct udev_enumerate *, const char *);
extern int udev_enumerate_scan_devices(struct udev_enumerate *);
extern struct udev_list_entry *udev_enumerate_get_list_entry(struct udev_enumerate *);
extern struct udev_list_entry *udev_list_entry_get_next(struct udev_list_entry *);
extern const char *udev_list_entry_get_name(struct udev_list_entry *);
extern struct udev_device *udev_device_new_from_syspath(struct udev *, const char *);
extern struct udev_device *udev_device_unref(struct udev_device *);
extern const char *udev_device_get_devnode(struct udev_device *);
extern const char *udev_device_get_subsystem(struct udev_device *);
extern const char *udev_device_get_sysname(struct udev_device *);
extern const char *udev_device_get_property_value(struct udev_device *, const char *);

static int print_device(struct udev *udev, const char *syspath, int *event0, int *event1)
{
    struct udev_device *device = udev_device_new_from_syspath(udev, syspath);
    if (device == NULL) {
        printf("  %s: new_from_syspath failed\n", syspath);
        return 0;
    }
    const char *name = udev_device_get_sysname(device);
    const char *subsystem = udev_device_get_subsystem(device);
    const char *node = udev_device_get_devnode(device);
    const char *input = udev_device_get_property_value(device, "ID_INPUT");
    const char *seat = udev_device_get_property_value(device, "ID_SEAT");
    printf("  %s name=%s subsystem=%s devnode=%s ID_INPUT=%s ID_SEAT=%s\n",
           syspath, name ? name : "-", subsystem ? subsystem : "-",
           node ? node : "-", input ? input : "-", seat ? seat : "-");
    if (name != NULL && strcmp(name, "event0") == 0)
        *event0 = node != NULL && strcmp(node, "/dev/input/event0") == 0
               && input != NULL && strcmp(input, "1") == 0;
    if (name != NULL && strcmp(name, "event1") == 0)
        *event1 = node != NULL && strcmp(node, "/dev/input/event1") == 0
               && input != NULL && strcmp(input, "1") == 0;
    udev_device_unref(device);
    return 1;
}

int main(void)
{
    struct udev *udev = udev_new();
    if (udev == NULL) {
        printf("udev_new failed\n");
        return 1;
    }
    errno = 0;
    struct udev_device *explicit = udev_device_new_from_syspath(udev, "/sys/class/input/event0");
    printf("explicit event0=%p errno=%d\n", (void *)explicit, errno);
    if (explicit != NULL) {
        const char *subsystem = udev_device_get_subsystem(explicit);
        const char *node = udev_device_get_devnode(explicit);
        const char *input = udev_device_get_property_value(explicit, "ID_INPUT");
        printf("  subsystem=%s devnode=%s ID_INPUT=%s\n",
               subsystem ? subsystem : "-", node ? node : "-", input ? input : "-");
        udev_device_unref(explicit);
    }
    struct udev_enumerate *enumerate = udev_enumerate_new(udev);
    if (enumerate == NULL) {
        printf("udev_enumerate_new failed\n");
        return 1;
    }
    udev_enumerate_add_match_subsystem(enumerate, "input");
    int scan = udev_enumerate_scan_devices(enumerate);
    printf("scan_devices=%d\n", scan);

    int event0 = 0;
    int event1 = 0;
    int count = 0;
    for (struct udev_list_entry *entry = udev_enumerate_get_list_entry(enumerate);
         entry != NULL; entry = udev_list_entry_get_next(entry)) {
        const char *syspath = udev_list_entry_get_name(entry);
        if (syspath != NULL) {
            count += print_device(udev, syspath, &event0, &event1);
        }
    }
    printf("enumerated=%d event0=%d event1=%d\n", count, event0, event1);
    udev_enumerate_unref(enumerate);
    udev_unref(udev);

    int ok = scan == 0 && event0 && event1;
    printf(ok ? "udev ok\n" : "udev FAILED\n");
    return ok ? 0 : 1;
}
