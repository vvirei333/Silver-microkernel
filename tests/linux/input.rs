/* Freestanding AArch64 Linux program: reads /dev/input/event0 and prints
 * each evdev event. Companion to the kernel's virtio_input driver.
 *
 *   linuxrun /sfs/linux-input
 *
 * Expected console output: one "ev t=X c=Y v=Z" line per event, then
 * "input ok" and exit 0. Runs non-blocking with a bounded wait so a missing
 * device or no events still exits cleanly (exit 1).
 */
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: i64 = 64;
const SYS_OPENAT: i64 = 56;
const SYS_READ: i64 = 63;
const SYS_NANOSLEEP: i64 = 101;
const SYS_EXIT: i64 = 93;
const AT_FDCWD: i64 = -100;
const O_RDONLY: i64 = 0;
const O_NONBLOCK: i64 = 0o4000;

#[inline(always)]
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

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    static PATH: &[u8] = b"/dev/input/event0\0";
    let fd = sys3(
        SYS_OPENAT,
        AT_FDCWD,
        PATH.as_ptr() as i64,
        O_RDONLY | O_NONBLOCK,
    );
    if fd < 0 {
        sys1(SYS_EXIT, 1);
    }

    let mut events = 0;
    let mut deadline = 0;
    let mut buffer = [0u8; 24];
    while events < 3 {
        let got = sys3(SYS_READ, fd, buffer.as_mut_ptr() as i64, 24);
        if got == 24 {
            // struct input_event: tv_sec u64, tv_usec u64, type u16,
            // code u16, value i32 — print type/code/value.
            let kind = u16::from_le_bytes([buffer[16], buffer[17]]);
            let code = u16::from_le_bytes([buffer[18], buffer[19]]);
            let value = i32::from_le_bytes([
                buffer[20], buffer[21], buffer[22], buffer[23],
            ]);
            print_ev(kind, code, value);
            events += 1;
        } else {
            // 10 ms sleep: {tv_sec: 0, tv_nsec: 10_000_000}.
            let timespec = [0i64, 10_000_000i64];
            deadline += 1;
            if deadline > 500 {
                sys1(SYS_EXIT, 1);
            }
            let _ = sys3(SYS_NANOSLEEP, timespec.as_ptr() as i64, 0, 0);
        }
    }
    loop {
        sys1(SYS_EXIT, 0);
    }

    fn sys1(n: i64, a: i64) -> i64 {
        sys3(n, a, 0, 0)
    }

    fn print_ev(kind: u16, code: u16, value: i32) {
        static EV: &[u8] = b"ev t=";
        sys3(SYS_WRITE, 1, EV.as_ptr() as i64, EV.len() as i64);
        print_hex(kind as u64);
        static C: &[u8] = b" c=";
        sys3(SYS_WRITE, 1, C.as_ptr() as i64, 3);
        print_hex(code as u64);
        static V: &[u8] = b" v=";
        sys3(SYS_WRITE, 1, V.as_ptr() as i64, 3);
        print_i32(value);
        static NL: &[u8] = b"\n";
        sys3(SYS_WRITE, 1, NL.as_ptr() as i64, 1);
    }

    fn print_hex(mut value: u64) {
        let mut digits = [0u8; 16];
        let mut length = 0;
        if value == 0 {
            digits[0] = b'0';
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
        let start = 16 - length;
        sys3(SYS_WRITE, 1, digits[start..].as_ptr() as i64, length as i64);
    }

    fn print_i32(value: i32) {
        let mut digits = [0u8; 12];
        let mut length = 0;
        let mut value = value;
        if value < 0 {
            static MINUS: &[u8] = b"-";
            sys3(SYS_WRITE, 1, MINUS.as_ptr() as i64, 1);
            value = -value;
        }
        if value == 0 {
            digits[0] = b'0';
            length = 1;
        }
        while value > 0 {
            digits[11 - length] = b'0' + (value % 10) as u8;
            length += 1;
            value /= 10;
        }
        let start = 12 - length;
        sys3(SYS_WRITE, 1, digits[start..].as_ptr() as i64, length as i64);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
