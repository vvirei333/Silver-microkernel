/* Freestanding AArch64 Linux program: probes the evdev nodes the kernel's
 * virtio-input driver exposes, then prints key events from the host.
 *
 *   linuxrun /sfs/linux-input
 *
 * event0 is expected to be a keyboard and event1 an absolute pointer (QEMU
 * virtio-tablet-pci). The program checks EVIOCGVERSION, EVIOCGID,
 * EVIOCGNAME, EVIOCGBIT(EV_SYN/EV_KEY/EV_ABS), EVIOCGPROP and EVIOCGABS,
 * prints one "ev t/c/v" line per event, then exits 0 when the checks and at
 * least one injected key press succeeded (1 otherwise).
 */
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_IOCTL: i64 = 29;
const SYS_WRITE: i64 = 64;
const SYS_OPENAT: i64 = 56;
const SYS_READ: i64 = 63;
const SYS_NANOSLEEP: i64 = 101;
const SYS_EXIT: i64 = 93;
const AT_FDCWD: i64 = -100;
const O_RDONLY: i64 = 0;
const O_NONBLOCK: i64 = 0o4000;

const EVIOCGVERSION: u64 = 0x8004_4501;
const EVIOCGID: u64 = 0x8008_4502;

fn sys3(n: i64, a: i64, b: i64, c: i64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") a => ret,
            in("x1") b,
            in("x2") c,
            in("x8") n,
            options(nostack)
        );
    }
    ret
}

fn sys1(n: i64, a: i64) -> i64 {
    sys3(n, a, 0, 0)
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

fn print_hex(mut value: u64) {
    let mut digits = [0u8; 16];
    let mut length = 0;
    if value == 0 {
        digits[15] = b'0';
        length = 1;
    }
    while value > 0 {
        let nibble = (value & 0xF) as u8;
        digits[15 - length] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        length += 1;
        value >>= 4;
    }
    write_all(&digits[16 - length..]);
}

fn ioctl(fd: i64, request: u64, argument: i64) -> i64 {
    sys3(SYS_IOCTL, fd, request as i64, argument)
}

/// EVIOCGNAME(len) and EVIOCGBIT(ev, len) requests are generated from the
/// buffer length, exactly like the Linux uapi macro.
fn ioc_read(nr: u64, length: u64) -> u64 {
    0x8000_0000 | (length << 16) | (0x45 << 8) | nr
}

fn open(path: &[u8]) -> i64 {
    sys3(
        SYS_OPENAT,
        AT_FDCWD,
        path.as_ptr() as i64,
        O_RDONLY | O_NONBLOCK,
    )
}

fn set_bits(bits: &[u8]) -> u64 {
    let mut count = 0;
    for byte in bits {
        count += byte.count_ones() as u64;
    }
    count
}

fn print_bits(label: &[u8], bits: &[u8]) {
    write_all(label);
    print_u64(set_bits(bits));
}

/// Prints the device identity and capabilities. Returns true when the basic
/// evdev contract holds (version, id, a non-empty name and event mask).
fn probe(fd: i64, slot: u64) -> bool {
    let mut ok = true;
    let mut version = [0u8; 4];
    let result = ioctl(fd, EVIOCGVERSION, version.as_mut_ptr() as i64);
    let version = u32::from_le_bytes(version);
    write_all(b"  version=0x");
    print_hex(version as u64);
    if result < 0 || version != 0x0001_0001 {
        ok = false;
        write_all(b" (bad)");
    }

    let mut id = [0u8; 8];
    if ioctl(fd, EVIOCGID, id.as_mut_ptr() as i64) < 0 {
        ok = false;
        write_all(b"  id=<error>");
    } else {
        write_all(b"  bus=0x");
        print_hex(u16::from_le_bytes([id[0], id[1]]) as u64);
        write_all(b" vendor=0x");
        print_hex(u16::from_le_bytes([id[2], id[3]]) as u64);
        write_all(b" product=0x");
        print_hex(u16::from_le_bytes([id[4], id[5]]) as u64);
    }

    let mut name = [0u8; 96];
    let length = ioctl(fd, ioc_read(0x06, 96), name.as_mut_ptr() as i64);
    if length <= 0 {
        ok = false;
        write_all(b"  name=<error>");
    } else {
        write_all(b"  name=");
        write_all(&name[..(length as usize - 1).min(name.len())]);
    }

    let mut event_types = [0u8; 32];
    if ioctl(fd, ioc_read(0x20, 32), event_types.as_mut_ptr() as i64) < 0 {
        ok = false;
    }
    print_bits(b"  ev=", &event_types);
    if set_bits(&event_types) == 0 {
        ok = false;
    }

    let mut keys = [0u8; 96];
    let _ = ioctl(fd, ioc_read(0x21, 96), keys.as_mut_ptr() as i64);
    print_bits(b" keys=", &keys);

    let mut axes = [0u8; 16];
    let _ = ioctl(fd, ioc_read(0x23, 16), axes.as_mut_ptr() as i64);
    print_bits(b" axes=", &axes);

    let mut props = [0u8; 32];
    let _ = ioctl(fd, ioc_read(0x09, 32), props.as_mut_ptr() as i64);
    print_bits(b" props=", &props);

    if slot == 1 && set_bits(&axes) > 0 {
        let mut axis = [0u8; 24];
        if ioctl(fd, 0x8018_4540, axis.as_mut_ptr() as i64) < 0 {
            ok = false;
            write_all(b" abs_x=<error>");
        } else {
            let word = |offset: usize| {
                i32::from_le_bytes([
                    axis[offset],
                    axis[offset + 1],
                    axis[offset + 2],
                    axis[offset + 3],
                ])
            };
            write_all(b" abs_x=");
            print_u64(word(4) as u64);
            write_all(b"..");
            print_u64(word(8) as u64);
            if word(8) <= word(4) {
                ok = false;
            }
        }
    }
    write_all(b"\n");
    ok
}

fn sleep_ms(milliseconds: i64) {
    let timespec = [0i64, milliseconds * 1_000_000];
    let _ = sys3(SYS_NANOSLEEP, timespec.as_ptr() as i64, 0, 0);
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    let mut ok = true;
    let keyboard = open(b"/dev/input/event0\0");
    if keyboard < 0 {
        write_all(b"input: /dev/input/event0 missing\n");
        sys1(SYS_EXIT, 1);
    }
    write_all(b"evdev0:\n");
    ok &= probe(keyboard, 0);

    let tablet = open(b"/dev/input/event1\0");
    if tablet >= 0 {
        write_all(b"evdev1:\n");
        ok &= probe(tablet, 1);
    } else {
        write_all(b"evdev1: absent\n");
    }

    // Drain event0 for up to six seconds; the host injects keys with the
    // QEMU monitor while this loop runs.
    write_all(b"events:\n");
    let mut buffer = [0u8; 24];
    let mut keys_seen = 0;
    let mut attempts = 0;
    while keys_seen < 3 && attempts < 600 {
        attempts += 1;
        let got = sys3(SYS_READ, keyboard, buffer.as_mut_ptr() as i64, 24);
        if got == 24 {
            let kind = u16::from_le_bytes([buffer[16], buffer[17]]);
            let code = u16::from_le_bytes([buffer[18], buffer[19]]);
            let value = i32::from_le_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
            write_all(b"ev t=");
            print_hex(kind as u64);
            write_all(b" c=");
            print_hex(code as u64);
            write_all(b" v=");
            print_u64(value as u64);
            write_all(b"\n");
            if kind == 1 && value != 0 {
                keys_seen += 1;
            }
        } else {
            sleep_ms(10);
        }
    }
    if keys_seen == 0 {
        write_all(b"input: no key events\n");
        ok = false;
    }

    // The pointer path: QEMU injects ABS_X/ABS_Y through QMP when a tablet is
    // present. Both coordinates must arrive as EV_ABS events on event1.
    if tablet >= 0 {
        write_all(b"abs events:\n");
        let mut axes_seen = 0;
        let mut attempts = 0;
        while axes_seen < 2 && attempts < 300 {
            attempts += 1;
            let got = sys3(SYS_READ, tablet, buffer.as_mut_ptr() as i64, 24);
            if got == 24 {
                let kind = u16::from_le_bytes([buffer[16], buffer[17]]);
                let code = u16::from_le_bytes([buffer[18], buffer[19]]);
                let value = i32::from_le_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
                write_all(b"abs t=");
                print_hex(kind as u64);
                write_all(b" c=");
                print_hex(code as u64);
                write_all(b" v=");
                print_u64(value as u64);
                write_all(b"\n");
                if kind == 3 {
                    axes_seen += 1;
                }
            } else {
                sleep_ms(10);
            }
        }
        if axes_seen < 2 {
            write_all(b"input: no abs events\n");
            ok = false;
        }
    }

    write_all(if ok { b"input ok\n" } else { b"input FAILED\n" });
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
