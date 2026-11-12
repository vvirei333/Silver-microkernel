//! Minimal synthetic sysfs for the DRM nodes.
//!
//! libdrm describes a DRM device by reading `/sys/dev/char/<major>:<minor>`:
//! the PCI vendor/device attributes and the `subsystem` symlink. This guest
//! has no PCI device tree and no sysfs, so `drmGetDevice2` fails, and Mesa's
//! loader refuses the render node ("failed to get compatible render device").
//!
//! The nodes below answer exactly those questions and nothing else. The values
//! match what a Linux guest with `virtio-gpu-pci` reports (1af4:1050, PCI bus,
//! slot 0000:00:04.0), so Mesa's driver table resolves to `virtio_gpu` and the
//! render node is accepted.

/// DRM character-device major.
pub const DRM_MAJOR: u32 = 226;
/// Minor of `/dev/dri/card0`.
pub const CARD_MINOR: u32 = 0;
/// Minor of `/dev/dri/renderD128`.
pub const RENDER_MINOR: u32 = 128;

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

/// One entry of the synthetic tree.
pub enum Node {
    File(&'static [u8]),
    Dir,
    Symlink(&'static str),
}

/// Matches `"226:128"` and returns the node's `dev` content.
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

/// Resolves an absolute path inside the synthetic tree.
pub fn lookup(path: &str) -> Option<Node> {
    let rest = path.strip_prefix("/sys/dev/char/")?;
    let (node, tail) = match rest.split_once('/') {
        Some((node, tail)) => (node, Some(tail)),
        None => (rest, None),
    };
    let minor = minor_of(node)?;
    let tail = tail.unwrap_or("");
    Some(match tail {
        "" => Node::Dir,
        "dev" => Node::File(if minor == RENDER_MINOR {
            RENDER_DEV
        } else {
            CARD_DEV
        }),
        // libdrm's `drmNodeIsDRM` only checks that this directory exists.
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

/// Content of a regular file node, for `read`.
pub fn file(path: &str) -> Option<&'static [u8]> {
    match lookup(path)? {
        Node::File(data) => Some(data),
        _ => None,
    }
}

/// Target of a symlink node, for `readlinkat`.
pub fn symlink(path: &str) -> Option<&'static str> {
    match lookup(path)? {
        Node::Symlink(target) => Some(target),
        _ => None,
    }
}

/// True for a directory node, so `stat` reports `S_IFDIR`.
pub fn is_dir(path: &str) -> bool {
    matches!(lookup(path), Some(Node::Dir))
}
