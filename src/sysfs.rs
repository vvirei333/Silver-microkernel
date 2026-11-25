//! Minimal synthetic sysfs and udev database.
//!
//! Linux userspace describes devices through sysfs. This guest has no PCI
//! tree and no udevd, so the nodes below are generated from the kernel's own
//! device state:
//!
//! * /sys/dev/char/226:0 and 226:128 answer libdrm's DRM questions
//!   (vendor/device ids, subsystem and the by-path node).
//! * /sys/class/input/eventN, /sys/devices/virtual/input/inputN and
//!   /sys/dev/char/13:64+N describe the virtio-input devices, including
//!   the dev, name and uevent attributes libudev reads.
//! * /run/udev/data/c13:64+N carries the properties udev's input rules
//!   would have written, so libudev/libinput accept the devices even though
//!   no udevd runs.
//!
//! Directory listings report an entry type because udev's enumerator filters
//! on d_type.

/// DRM character-device major.
pub const DRM_MAJOR: u32 = 226;
/// Minor of /dev/dri/card0.
pub const CARD_MINOR: u32 = 0;
/// Minor of /dev/dri/renderD128.
pub const RENDER_MINOR: u32 = 128;

/// evdev character-device major (Linux's INPUT_MAJOR).
pub const INPUT_MAJOR: u32 = 13;
/// Minor of event0; eventN uses MINOR_BASE + N.
pub const INPUT_MINOR_BASE: u32 = 64;

/// d_type values reported by getdents64.
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;

const VENDOR: &[u8] = b"0x1af4\n";
const DEVICE: &[u8] = b"0x1050\n";
const SUBSYSTEM_VENDOR: &[u8] = b"0x1af4\n";
const SUBSYSTEM_DEVICE: &[u8] = b"0x1100\n";
const UEVENT: &[u8] = b"DRIVER=virtio-pci\n\
PCI_CLASS=38000\n\
PCI_ID=1AF4:1050\n\
PCI_SUBSYS_ID=1AF4:1100\n\
PCI_SLOT_NAME=0000:00:04.0\n\
MODALIAS=pci:v00001AF4d00001050sv00001AF4sd00001100bc03sc80i00\n";

const CARD_DEV: &[u8] = b"226:0\n";
const RENDER_DEV: &[u8] = b"226:128\n";
const EVENT0_DEV: &[u8] = b"13:64\n";
const EVENT1_DEV: &[u8] = b"13:65\n";

const INPUT0_PATH: &str = "/sys/devices/virtual/input/input0";
const INPUT1_PATH: &str = "/sys/devices/virtual/input/input1";

/// One entry of a synthetic directory.
pub struct Entry {
    pub name: &'static str,
    pub kind: u8,
}

const SYS_ROOT: &[Entry] = &[
    Entry { name: "class", kind: DT_DIR },
    Entry { name: "dev", kind: DT_DIR },
    Entry { name: "devices", kind: DT_DIR },
];
const CLASS_ROOT: &[Entry] = &[Entry { name: "input", kind: DT_DIR }];
const DEV_ROOT: &[Entry] = &[Entry { name: "char", kind: DT_DIR }];
const DEVICES_ROOT: &[Entry] = &[Entry { name: "virtual", kind: DT_DIR }];
const VIRTUAL_ROOT: &[Entry] = &[Entry { name: "input", kind: DT_DIR }];
const CHAR_ROOT: &[Entry] = &[
    Entry { name: "226:0", kind: DT_REG },
    Entry { name: "226:128", kind: DT_REG },
    Entry { name: "13:64", kind: DT_REG },
];
const CHAR_ROOT_TWO: &[Entry] = &[
    Entry { name: "226:0", kind: DT_REG },
    Entry { name: "226:128", kind: DT_REG },
    Entry { name: "13:64", kind: DT_REG },
    Entry { name: "13:65", kind: DT_REG },
];
const CHAR_ENTRIES: &[Entry] = &[
    Entry { name: "dev", kind: DT_REG },
    Entry { name: "uevent", kind: DT_REG },
    Entry { name: "subsystem", kind: DT_REG },
    Entry { name: "device", kind: DT_REG },
];
const EVENT_ENTRIES: &[Entry] = &[
    Entry { name: "dev", kind: DT_REG },
    Entry { name: "name", kind: DT_REG },
    Entry { name: "uevent", kind: DT_REG },
    Entry { name: "subsystem", kind: DT_REG },
    Entry { name: "device", kind: DT_REG },
];
const INPUT0_ENTRIES: &[Entry] = &[
    Entry { name: "name", kind: DT_REG },
    Entry { name: "uevent", kind: DT_REG },
    Entry { name: "event0", kind: DT_DIR },
];
const INPUT1_ENTRIES: &[Entry] = &[
    Entry { name: "name", kind: DT_REG },
    Entry { name: "uevent", kind: DT_REG },
    Entry { name: "event1", kind: DT_DIR },
];
const INPUT_ROOT_ONE: &[Entry] = &[Entry { name: "input0", kind: DT_DIR }];
const INPUT_ROOT_TWO: &[Entry] = &[
    Entry { name: "input0", kind: DT_DIR },
    Entry { name: "input1", kind: DT_DIR },
];
const CLASS_INPUT_ONE: &[Entry] = &[Entry { name: "event0", kind: DT_DIR }];
const CLASS_INPUT_TWO: &[Entry] = &[
    Entry { name: "event0", kind: DT_DIR },
    Entry { name: "event1", kind: DT_DIR },
];

const UDEV_INPUT0: &[u8] = b"I:1\n\
    E:ID_INPUT=1\n\
    E:ID_INPUT_KEYBOARD=1\n\
    E:ID_SEAT=seat0\n\
    E:ID_PATH=platform-virtio-input0\n";
const UDEV_INPUT1: &[u8] = b"I:2\n\
    E:ID_INPUT=1\n\
    E:ID_INPUT_MOUSE=1\n\
    E:ID_SEAT=seat0\n\
    E:ID_PATH=platform-virtio-input1\n";

/// One entry of the synthetic tree.
pub enum Node {
    File(&'static [u8]),
    Dir,
    Symlink(&'static str),
}

/// Matches "226:128" and returns the DRM node minor.
fn minor_of(text: &str) -> Option<u32> {
    let (major, minor) = text.split_once(':')?;
    if major.parse::<u32>().ok()? != DRM_MAJOR {
        return None;
    }
    let minor = minor.parse::<u32>().ok()?;
    if minor == CARD_MINOR || minor == RENDER_MINOR {
        Some(minor)
    } else {
        None
    }
}

/// Matches "13:64" / "13:65" and returns the event index.
fn input_minor_of(text: &str) -> Option<usize> {
    let (major, minor) = text.split_once(':')?;
    if major.parse::<u32>().ok()? != INPUT_MAJOR {
        return None;
    }
    let minor = minor.parse::<u32>().ok()?;
    let index = minor.checked_sub(INPUT_MINOR_BASE)? as usize;
    if index < 2 && input_present(index) {
        Some(index)
    } else {
        None
    }
}

fn input_present(index: usize) -> bool {
    index < crate::virtio_input::count()
}

fn input_dev_file(index: usize) -> &'static [u8] {
    if index == 0 {
        EVENT0_DEV
    } else {
        EVENT1_DEV
    }
}

fn input_path(index: usize) -> &'static str {
    if index == 0 {
        INPUT0_PATH
    } else {
        INPUT1_PATH
    }
}

/// Resolves the children of /sys/class/input/eventN.
fn event_node(index: usize, tail: &str) -> Option<Node> {
    Some(match tail {
        "" => Node::Dir,
        "dev" => Node::File(input_dev_file(index)),
        "name" => Node::File(crate::virtio_input::sysfs_name(index)),
        "uevent" => Node::File(crate::virtio_input::sysfs_uevent(index)),
        "subsystem" => Node::Symlink("/sys/class/input"),
        "device" => Node::Symlink(input_path(index)),
        _ => return None,
    })
}

fn class_input(rest: &str) -> Option<Node> {
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Node::Dir);
    }
    let (name, tail) = match rest.split_once('/') {
        Some((name, tail)) => (name, Some(tail)),
        None => (rest, None),
    };
    let index = name.strip_prefix("event")?.parse::<usize>().ok()?;
    if !input_present(index) {
        return None;
    }
    event_node(index, tail.unwrap_or(""))
}

fn virtual_input(rest: &str) -> Option<Node> {
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Node::Dir);
    }
    let (name, tail) = match rest.split_once('/') {
        Some((name, tail)) => (name, Some(tail)),
        None => (rest, None),
    };
    let index = name.strip_prefix("input")?.parse::<usize>().ok()?;
    if !input_present(index) {
        return None;
    }
    let tail = tail.unwrap_or("");
    match tail {
        "" => Some(Node::Dir),
        "name" => Some(Node::File(crate::virtio_input::sysfs_name(index))),
        "uevent" => Some(Node::File(crate::virtio_input::sysfs_uevent(index))),
        _ => {
            let event = if index == 0 { "event0" } else { "event1" };
            let event_tail = tail.strip_prefix(event)?;
            if !event_tail.is_empty() && !event_tail.starts_with('/') {
                return None;
            }
            event_node(index, event_tail.strip_prefix('/').unwrap_or(event_tail))
        }
    }
}

/// Resolves paths under /sys/dev/char: DRM first, then evdev.
fn char_node(rest: &str) -> Option<Node> {
    let (node, tail) = match rest.split_once('/') {
        Some((node, tail)) => (node, Some(tail)),
        None => (rest, None),
    };
    if let Some(index) = input_minor_of(node) {
        let tail = tail.unwrap_or("");
        return match tail {
            // libinput maps an open evdev fd back to a syspath through
            // udev_device_new_from_devnum(); that path must be the same
            // symlink target as /sys/class/input/eventN.
            "" => Some(Node::Symlink(if index == 0 {
                "/sys/class/input/event0"
            } else {
                "/sys/class/input/event1"
            })),
            "dev" => Some(Node::File(input_dev_file(index))),
            "uevent" => Some(Node::File(crate::virtio_input::sysfs_uevent(index))),
            "subsystem" => Some(Node::Symlink("/sys/class/input")),
            "device" => Some(Node::Symlink(input_path(index))),
            _ => None,
        };
    }
    let minor = minor_of(node)?;
    let tail = tail.unwrap_or("");
    Some(match tail {
        "" => Node::Dir,
        "dev" => Node::File(if minor == RENDER_MINOR {
            RENDER_DEV
        } else {
            CARD_DEV
        }),
        // libdrm's drmNodeIsDRM only checks that this directory exists.
        "device/drm" => Node::Dir,
        "device/drm/card0" => Node::Dir,
        "device" => Node::Dir,
        "device/vendor" => Node::File(VENDOR),
        "device/device" => Node::File(DEVICE),
        "device/subsystem_vendor" => Node::File(SUBSYSTEM_VENDOR),
        "device/subsystem_device" => Node::File(SUBSYSTEM_DEVICE),
        "device/uevent" => Node::File(UEVENT),
        "device/subsystem" => Node::Symlink("/sys/bus/pci"),
        _ => return None,
    })
}

/// Resolves the udev database shim. A real udevd would create these files
/// from its rules; without a daemon the kernel supplies the two properties
/// libinput insists on: ID_INPUT and a seat.
fn udev_data(rest: &str) -> Option<Node> {
    match rest {
        "c13:64" if input_present(0) => Some(Node::File(UDEV_INPUT0)),
        "c13:65" if input_present(1) => Some(Node::File(UDEV_INPUT1)),
        _ => None,
    }
}

/// Resolves an absolute path inside the synthetic tree.
pub fn lookup(path: &str) -> Option<Node> {
    if let Some(rest) = path.strip_prefix("/sys/dev/char/") {
        return char_node(rest);
    }
    if let Some(rest) = path.strip_prefix("/sys/class/input") {
        return class_input(rest);
    }
    if let Some(rest) = path.strip_prefix("/sys/devices/virtual/input") {
        return virtual_input(rest);
    }
    // `/run` is a RamFs alias to `/sfs/run`, so the resolver hands us the
    // underlying path. Serve the database from both spellings.
    if let Some(rest) = path
        .strip_prefix("/run/udev/data/")
        .or_else(|| path.strip_prefix("/sfs/run/udev/data/"))
    {
        return udev_data(rest);
    }
    Some(match path {
        "/sys" | "/sys/class" | "/sys/dev" | "/sys/dev/char" | "/sys/devices"
        | "/sys/devices/virtual" | "/sys/devices/virtual/input" | "/run" | "/run/udev"
        | "/run/udev/data" | "/sfs/run/udev" | "/sfs/run/udev/data" => Node::Dir,
        _ => return None,
    })
}

/// Directory entries for getdents64, with their d_type.
pub fn entries(path: &str) -> &'static [Entry] {
    match path {
        "/sys" => SYS_ROOT,
        "/sys/class" => CLASS_ROOT,
        "/sys/class/input" => match crate::virtio_input::count() {
            0 => &[],
            1 => CLASS_INPUT_ONE,
            _ => CLASS_INPUT_TWO,
        },
        "/sys/dev" => DEV_ROOT,
        "/sys/dev/char" => {
            if crate::virtio_input::count() > 1 {
                CHAR_ROOT_TWO
            } else {
                CHAR_ROOT
            }
        }
        "/sys/devices" => DEVICES_ROOT,
        "/sys/devices/virtual" => VIRTUAL_ROOT,
        "/sys/devices/virtual/input" => match crate::virtio_input::count() {
            0 => &[],
            1 => INPUT_ROOT_ONE,
            _ => INPUT_ROOT_TWO,
        },
        "/sys/devices/virtual/input/input0" => INPUT0_ENTRIES,
        "/sys/devices/virtual/input/input1" => INPUT1_ENTRIES,
        "/sys/devices/virtual/input/input0/event0"
        | "/sys/devices/virtual/input/input1/event1" => EVENT_ENTRIES,
        "/sys/class/input/event0" | "/sys/class/input/event1" => EVENT_ENTRIES,
        _ => {
            if path.starts_with("/sys/dev/char/") {
                CHAR_ENTRIES
            } else {
                &[]
            }
        }
    }
}

/// Content of a regular file node, for read.
pub fn file(path: &str) -> Option<&'static [u8]> {
    match lookup(path)? {
        Node::File(data) => Some(data),
        _ => None,
    }
}

/// Target of a symlink node, for readlinkat.
pub fn symlink(path: &str) -> Option<&'static str> {
    match lookup(path)? {
        Node::Symlink(target) => Some(target),
        _ => None,
    }
}

/// True for a directory node, so stat reports S_IFDIR.
pub fn is_dir(path: &str) -> bool {
    matches!(lookup(path), Some(Node::Dir))
}
