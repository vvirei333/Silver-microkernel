//! Minimal synthetic procfs.
//!
//! OpenRC's sysinit reads a handful of `/proc` files (`cmdline`,
//! `filesystems`, `devices`, `mounts`, `modules`) and service scripts probe
//! `/proc/<pid>` for liveness. There is no procfs backend in the kernel, so
//! the Linux ABI answers exactly those names from a static table, the same way
//! [`crate::sysfs`] answers libdrm.
//!
//! This is deliberately not a real procfs: values are static, nothing is
//! writable, and per-process content describes the init process only.

pub enum Node {
    File(&'static [u8]),
    Dir(&'static [&'static str]),
    Symlink(&'static str),
}

const CMDLINE: &[u8] = b"console=ttyAMA0,115200 root=/dev/vda rw init=/sbin/init\0";
const VERSION: &[u8] =
    b"Linux version 6.6.0-werewolf (silver@werewolf) (rustc) #1 SMP PREEMPT aarch64\n";
const UPTIME: &[u8] = b"0.00 0.00\n";
const LOADAVG: &[u8] = b"0.00 0.00 0.00 1/1 1\n";
const FILESYSTEMS: &[u8] = b"nodev\tsysfs\nnodev\tproc\nnodev\ttmpfs\nnodev\tdevtmpfs\nnodev\tsfs\n\text4\n";
const MOUNTS: &[u8] = b"rootfs / rootfs rw 0 0\n\
sfs /sfs sfs rw 0 0\n\
proc /proc proc rw 0 0\n\
sysfs /sys sysfs rw 0 0\n\
devtmpfs /dev devtmpfs rw 0 0\n\
tmpfs /run tmpfs rw 0 0\n";
const MOUNTINFO: &[u8] = b"21 1 0:1 / / rw,relatime - rootfs rootfs rw\n\
22 21 0:2 / /sfs rw,relatime - sfs sfs rw\n\
23 21 0:3 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n\
24 21 0:4 / /sys rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw\n\
25 21 0:5 / /dev rw,nosuid,relatime - devtmpfs devtmpfs rw\n\
26 21 0:6 / /run rw,nosuid,nodev,relatime - tmpfs tmpfs rw\n";
const DEVICES: &[u8] = b"Character devices:\n\
  1 mem\n\
  5 /dev/tty\n\
  5 /dev/console\n\
  5 /dev/ptmx\n\
  1 /dev/null\n\
  1 /dev/zero\n\
  1 /dev/random\n\
  1 /dev/urandom\n\
226 drm\n";
const MEMINFO: &[u8] = b"MemTotal:        1024000 kB\n\
MemFree:          512000 kB\n\
MemAvailable:     768000 kB\n\
Buffers:               0 kB\n\
Cached:                0 kB\n\
SwapTotal:             0 kB\n\
SwapFree:              0 kB\n";
const CGROUPS: &[u8] = b"#subsys_name\thierarchy\tnum_cgroups\tenabled\n";
const MODULES: &[u8] = b"";
const STAT: &[u8] = b"cpu  0 0 0 0 0 0 0 0 0 0\nctxt 0\nbtime 0\nprocesses 0\n";
const STATUS: &[u8] = b"Name:\tinit\n\
Umask:\t0022\n\
State:\tS (sleeping)\n\
Tgid:\t1\n\
Pid:\t1\n\
PPid:\t0\n\
Uid:\t0\t0\t0\t0\n\
Gid:\t0\t0\t0\t0\n\
Threads:\t1\n";
const SELF_CMDLINE: &[u8] = b"/sbin/init\0";
const SELF_COMM: &[u8] = b"init\n";
const HOTPLUG: &[u8] = b"/sbin/mdev\n";

const ROOT: &[&str] = &[
    "cmdline",
    "filesystems",
    "mounts",
    "version",
    "uptime",
    "loadavg",
    "devices",
    "meminfo",
    "cgroups",
    "modules",
    "stat",
    "self",
    "1",
    "sys",
];
const PROCESS: &[&str] = &["cmdline", "comm", "status", "fd", "mountinfo", "mounts"];
const FD: &[&str] = &["0", "1", "2"];
const SYS: &[&str] = &["kernel"];
const SYS_KERNEL: &[&str] = &["hotplug"];

fn process_file(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "cmdline" => SELF_CMDLINE,
        "comm" => SELF_COMM,
        "status" => STATUS,
        "mountinfo" => MOUNTINFO,
        "mounts" => MOUNTS,
        _ => return None,
    })
}

/// Resolves an absolute path inside the synthetic tree.
pub fn lookup(path: &str) -> Option<Node> {
    let rest = path.strip_prefix("/proc")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let head = match parts.next() {
        None => return Some(Node::Dir(ROOT)),
        Some(head) => head,
    };
    let second = parts.next();
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    match (head, second, third) {
        (name @ ("cmdline" | "filesystems" | "mounts" | "version" | "uptime"
        | "loadavg" | "devices" | "meminfo" | "cgroups" | "modules" | "stat"), None, None) => {
            let data = match name {
                "cmdline" => CMDLINE,
                "filesystems" => FILESYSTEMS,
                "mounts" => MOUNTS,
                "version" => VERSION,
                "uptime" => UPTIME,
                "loadavg" => LOADAVG,
                "devices" => DEVICES,
                "meminfo" => MEMINFO,
                "cgroups" => CGROUPS,
                "modules" => MODULES,
                _ => STAT,
            };
            Some(Node::File(data))
        }
        ("self" | "1", None, None) => Some(Node::Dir(PROCESS)),
        ("self" | "1", Some(name), None) => process_file(name).map(Node::File),
        ("self" | "1", Some("fd"), Some(name)) => match name {
            "0" | "1" | "2" => Some(Node::Symlink("/dev/console")),
            _ => None,
        },
        ("self" | "1", Some("fd"), None) => Some(Node::Dir(FD)),
        ("sys", None, None) => Some(Node::Dir(SYS)),
        ("sys", Some("kernel"), None) => Some(Node::Dir(SYS_KERNEL)),
        ("sys", Some("kernel"), Some("hotplug")) => Some(Node::File(HOTPLUG)),
        _ => None,
    }
}

/// Directory listing for `getdents64`, if `path` is a synthetic directory.
pub fn entries(path: &str) -> Option<&'static [&'static str]> {
    match lookup(path)? {
        Node::Dir(names) => Some(names),
        _ => None,
    }
}
