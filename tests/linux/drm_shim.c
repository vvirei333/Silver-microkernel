/* Minimal libdrm device-description shim.
 *
 * Mesa's loader asks libdrm for the device behind an fd (`drmGetDevice2`) and
 * matches the PCI ids against its driver table. libdrm answers that by
 * reading /sys/dev/char/<maj>:<min>/device, which this guest does not have:
 * /dev/dri/* are synthetic nodes backed by the kernel's DRM implementation,
 * not by a PCI device tree.
 *
 * This library provides the handful of enumeration entry points Mesa uses and
 * describes the device exactly as a Linux guest with virtio-gpu would see it
 * (1af4:1050, PCI bus, primary + render nodes). Everything else (ioctls,
 * version, caps) still comes from the real libdrm.
 *
 * Load it before libdrm (LD_PRELOAD, or dlopen with RTLD_GLOBAL first).
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define DRM_NODE_PRIMARY 0
#define DRM_NODE_CONTROL 1
#define DRM_NODE_RENDER 2
#define DRM_NODE_MAX 3

typedef enum {
    DRM_BUS_PCI = 0,
    DRM_BUS_USB = 1,
    DRM_BUS_PLATFORM = 2,
    DRM_BUS_HOST1X = 3,
    DRM_BUS_VIRTIO = 4,
} drmDeviceBusType;

typedef struct _drmPciDeviceInfo {
    uint16_t vendor_id;
    uint16_t device_id;
    uint16_t subvendor_id;
    uint16_t subdevice_id;
    uint8_t revision_id;
} drmPciDeviceInfo, *drmPciDeviceInfoPtr;

typedef struct _drmDevice {
    char **nodes;
    int available_nodes;
    drmDeviceBusType bustype;
    union {
        drmPciDeviceInfoPtr pci;
        void *usb;
        void *platform;
        void *host1x;
        void *virtio;
    } businfo;
    union {
        drmPciDeviceInfoPtr pci;
        void *usb;
        void *platform;
        void *host1x;
        void *virtio;
    } deviceinfo;
} drmDevice, *drmDevicePtr;

static drmPciDeviceInfoPtr make_pci_info(void) {
    drmPciDeviceInfoPtr info = calloc(1, sizeof(*info));
    if (info == NULL) {
        return NULL;
    }
    info->vendor_id = 0x1af4;
    info->device_id = 0x1050;
    info->subvendor_id = 0x1af4;
    info->subdevice_id = 0x1100;
    info->revision_id = 1;
    return info;
}

static drmDevicePtr make_device(void) {
    drmDevicePtr device = calloc(1, sizeof(*device));
    if (device == NULL) {
        return NULL;
    }
    device->nodes = calloc(DRM_NODE_MAX, sizeof(char *));
    if (device->nodes == NULL) {
        free(device);
        return NULL;
    }
    device->nodes[DRM_NODE_PRIMARY] = strdup("/dev/dri/card0");
    device->nodes[DRM_NODE_CONTROL] = strdup("/dev/dri/controlD64");
    device->nodes[DRM_NODE_RENDER] = strdup("/dev/dri/renderD128");
    device->available_nodes = (1 << DRM_NODE_PRIMARY) | (1 << DRM_NODE_RENDER);
    device->bustype = DRM_BUS_PCI;
    device->businfo.pci = make_pci_info();
    device->deviceinfo.pci = make_pci_info();
    if (device->businfo.pci == NULL || device->deviceinfo.pci == NULL) {
        return NULL;
    }
    return device;
}

int drmGetDevice2(int fd, uint32_t flags, drmDevicePtr *device) {
    (void)flags;
    if (device == NULL || fd < 0) {
        return -22; /* EINVAL */
    }
    drmDevicePtr created = make_device();
    if (created == NULL) {
        return -12; /* ENOMEM */
    }
    *device = created;
    return 0;
}

int drmGetDeviceFromDevId(dev_t dev_id, uint32_t flags, drmDevicePtr *device) {
    (void)dev_id;
    return drmGetDevice2(0, flags, device);
}

int drmGetDevices2(uint32_t flags, drmDevicePtr devices[], int max_devices) {
    (void)flags;
    if (devices == NULL || max_devices < 1) {
        return 0;
    }
    drmDevicePtr created = make_device();
    if (created == NULL) {
        return -12;
    }
    devices[0] = created;
    return 1;
}

void drmFreeDevice(drmDevicePtr *device) {
    if (device == NULL || *device == NULL) {
        return;
    }
    drmDevicePtr entry = *device;
    if (entry->nodes != NULL) {
        for (int index = 0; index < DRM_NODE_MAX; index++) {
            free(entry->nodes[index]);
        }
        free(entry->nodes);
    }
    free(entry->businfo.pci);
    free(entry->deviceinfo.pci);
    free(entry);
    *device = NULL;
}

void drmFreeDevices(drmDevicePtr devices[], int count) {
    if (devices == NULL) {
        return;
    }
    for (int index = 0; index < count; index++) {
        drmFreeDevice(&devices[index]);
    }
}

int drmDevicesEqual(drmDevicePtr a, drmDevicePtr b) {
    if (a == NULL || b == NULL) {
        return 0;
    }
    if (a->bustype != b->bustype) {
        return 0;
    }
    if (a->businfo.pci == NULL || b->businfo.pci == NULL) {
        return 0;
    }
    return a->businfo.pci->vendor_id == b->businfo.pci->vendor_id &&
           a->businfo.pci->device_id == b->businfo.pci->device_id;
}

int drmGetNodeTypeFromFd(int fd) {
    (void)fd;
    return DRM_NODE_RENDER;
}

char *drmGetDeviceNameFromFd2(int fd) {
    (void)fd;
    return strdup("/dev/dri/renderD128");
}

char *drmGetPrimaryDeviceNameFromFd(int fd) {
    (void)fd;
    return strdup("/dev/dri/card0");
}

char *drmGetRenderDeviceNameFromFd(int fd) {
    (void)fd;
    return strdup("/dev/dri/renderD128");
}
