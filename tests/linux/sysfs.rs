/* Freestanding AArch64 Linux program: walks the synthetic sysfs input class
 * and the udev database shim, the metadata libudev/libinput need.
 *
 *   linuxrun /sfs/linux-sysfs
 *
 * Checks readdir of /sys/class/input (event0 + event1 with d_type), the
 * per-device dev/name/uevent attributes, the /sys/dev/char/13:M aliases,
 * the subsystem/device symlinks and /run/udev/data/c13:64. Exits 0 when
 * every check passes.
 */
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_READLINKAT: i64 = 78;
const SYS_GETDENTS64: i64 = 61;
const SYS_OPENAT: i64 = 56;
const SYS_READ: i64 = 63;
const SYS_WRITE: i64 = 64;
const SYS_CLOSE: i64 = 57;
const SYS_EXIT: i64 = 93;
const AT_FDCWD: i64 = -100;
const O_RDONLY: i64 = 0;
const O_DIRECTORY: i64 = 0o40000;
const DT_DIR: u8 = 4;

fn sys4(n: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") a => ret,
            in("x1") b,
            in("x2") c,
            in("x3") d,
            in("x8") n,
            options(nostack)
        );
    }
    ret
}

fn sys3(n: i64, a: i64, b: i64, c: i64) -> i64 {
    sys4(n, a, b, c, 0)
}

fn sys1(n: i64, a: i64) -> i64 {
    sys4(n, a, 0, 0, 0)
}

fn write_all(bytes: &[u8]) {
    sys3(SYS_WRITE, 1, bytes.as_ptr() as i64, bytes.len() as i64);
}

fn print_u64(mut value: u64) {
    let mut digits = [0u8; 20];
    let mut length = 0;
    if value == 0 {
        digits[19] = b'0';
        length = 1;
    }
    while value > 0 {
        digits[19 - length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
    }
    write_all(&digits[20 - length..]);
}

fn open(path: &[u8], flags: i64) -> i64 {
    sys3(SYS_OPENAT, AT_FDCWD, path.as_ptr() as i64, flags)
}

fn close(fd: i64) {
    sys1(SYS_CLOSE, fd);
}

/// Reads a whole small file. Returns the byte count or -1.
fn read_file(path: &[u8], out: &mut [u8]) -> i64 {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return -1;
    }
    let mut total = 0usize;
    loop {
        if total == out.len() {
            break;
        }
        let got = sys3(
            SYS_READ,
            fd,
            out[total..].as_mut_ptr() as i64,
            (out.len() - total) as i64,
        );
        if got < 0 {
            close(fd);
            return -1;
        }
        if got == 0 {
            break;
        }
        total += got as usize;
    }
    close(fd);
    total as i64
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let mut start = 0;
    while start + needle.len() <= haystack.len() {
        if &haystack[start..start + needle.len()] == needle {
            return true;
        }
        start += 1;
    }
    false
}

fn join<'a>(prefix: &[u8], suffix: &[u8], out: &'a mut [u8; 128]) -> &'a [u8] {
    let mut length = 0;
    for byte in prefix.iter().chain(suffix.iter()) {
        out[length] = *byte;
        length += 1;
    }
    out[length] = 0;
    &out[..length + 1]
}

fn expect(ok: bool, label: &[u8]) -> bool {
    write_all(b"  ");
    write_all(label);
    write_all(if ok { b": ok\n" } else { b": FAIL\n" });
    ok
}

/// Prints and checks one event node: dev, name, uevent and the subsystem
/// symlink.
fn check_event(
    prefix: &[u8],
    device: &[u8],
    name_part: &[u8],
    uevent_part: &[u8],
) -> bool {
    let mut path = [0u8; 128];
    let mut buffer = [0u8; 256];
    let mut ok = true;

    let dev_path = join(prefix, b"dev", &mut path);
    let length = read_file(dev_path, &mut buffer);
    write_all(b"  dev=");
    if length > 0 {
        write_all(&buffer[..length as usize]);
    } else {
        write_all(b"<error>");
    }
    ok &= expect(length > 0 && &buffer[..length as usize] == device, b"dev matches");

    let name_path = join(prefix, b"name", &mut path);
    let length = read_file(name_path, &mut buffer);
    write_all(b"  name=");
    if length > 0 {
        write_all(&buffer[..length as usize]);
    } else {
        write_all(b"<error>");
    }
    ok &= expect(length > 0 && contains(&buffer[..length as usize], name_part), b"name matches");

    let uevent_path = join(prefix, b"uevent", &mut path);
    let length = read_file(uevent_path, &mut buffer);
    ok &= expect(
        length > 0 && contains(&buffer[..length as usize], uevent_part),
        b"uevent matches",
    );

    let subsystem_path = join(prefix, b"subsystem", &mut path);
    let target = sys4(
        SYS_READLINKAT,
        AT_FDCWD,
        subsystem_path.as_ptr() as i64,
        buffer.as_mut_ptr() as i64,
        buffer.len() as i64,
    );
    ok &= expect(
        target > 0 && &buffer[..target as usize] == b"/sys/class/input",
        b"subsystem link",
    );
    ok
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    let mut ok = true;

    // The class directory: udev's enumerator scans it and filters on d_type.
    write_all(b"sysfs input:\n");
    let dir = open(b"/sys/class/input\0", O_RDONLY | O_DIRECTORY);
    let mut entries = [0u8; 512];
    let mut event0 = false;
    let mut event1 = false;
    if dir < 0 {
        write_all(b"  open /sys/class/input: FAIL\n");
        ok = false;
    } else {
        let length = sys3(
            SYS_GETDENTS64,
            dir,
            entries.as_mut_ptr() as i64,
            entries.len() as i64,
        );
        close(dir);
        if length < 0 {
            write_all(b"  getdents: FAIL\n");
            ok = false;
        } else {
            let mut offset = 0usize;
            while offset + 19 <= length as usize {
                let record = u16::from_le_bytes([entries[offset + 16], entries[offset + 17]]) as usize;
                if record == 0 || offset + record > length as usize {
                    break;
                }
                let kind = entries[offset + 18];
                let mut end = offset + 19;
                while end < offset + record && entries[end] != 0 {
                    end += 1;
                }
                let name = &entries[offset + 19..end];
                write_all(b"  entry ");
                write_all(name);
                write_all(b" type=");
                print_u64(kind as u64);
                write_all(b"\n");
                if name == b"event0" {
                    event0 = kind == DT_DIR;
                }
                if name == b"event1" {
                    event1 = kind == DT_DIR;
                }
                offset += record;
            }
        }
    }
    ok &= expect(event0, b"event0 listed");
    ok &= expect(event1, b"event1 listed");

    ok &= check_event(
        b"/sys/class/input/event0/",
        b"13:64\n",
        b"Keyboard",
        b"DEVNAME=input/event0",
    );
    ok &= check_event(
        b"/sys/class/input/event1/",
        b"13:65\n",
        b"Tablet",
        b"DEVNAME=input/event1",
    );

    // /sys/dev/char is the reverse mapping libudev uses for a devnode.
    let mut path = [0u8; 128];
    let mut buffer = [0u8; 256];
    let char_path = join(b"/sys/dev/char/13:65/", b"uevent", &mut path);
    let length = read_file(char_path, &mut buffer);
    ok &= expect(
        length > 0 && contains(&buffer[..length as usize], b"DEVNAME=input/event1"),
        b"char uevent",
    );

    let device_path = join(b"/sys/dev/char/13:65/", b"device", &mut path);
    let target = sys4(
        SYS_READLINKAT,
        AT_FDCWD,
        device_path.as_ptr() as i64,
        buffer.as_mut_ptr() as i64,
        buffer.len() as i64,
    );
    ok &= expect(
        target > 0 && &buffer[..target as usize] == b"/sys/devices/virtual/input/input1",
        b"char device link",
    );

    // The udev database shim supplies the property libinput requires.
    let length = read_file(b"/run/udev/data/c13:64\0", &mut buffer);
    write_all(b"  udev db=");
    if length > 0 {
        write_all(&buffer[..length as usize]);
    } else {
        write_all(b"<error>");
    }
    ok &= expect(
        length > 0 && contains(&buffer[..length as usize], b"ID_INPUT=1"),
        b"udev ID_INPUT",
    );

    write_all(if ok { b"sysfs ok\n" } else { b"sysfs FAILED\n" });
    sys1(SYS_EXIT, if ok { 0 } else { 1 });
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
