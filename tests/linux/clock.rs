/* Freestanding AArch64 Linux program: checks that the time ABI separates the
 * wall clock from the monotonic tick count.
 *
 *   linuxrun /sfs/linux-clock
 *
 * The kernel samples the PL031 RTC at boot (QEMU virt, epoch set from the
 * host clock) and CLOCK_REALTIME must report that epoch plus uptime, while
 * CLOCK_MONOTONIC stays small. `gettimeofday` must agree with CLOCK_REALTIME.
 * Prints one summary line, then "clock ok" (exit 0) or "clock FAILED"
 * (exit 1).
 */
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: i64 = 64;
const SYS_CLOCK_GETTIME: i64 = 113;
const SYS_GETTIMEOFDAY: i64 = 169;
const SYS_EXIT: i64 = 93;
const CLOCK_REALTIME: i64 = 0;
const CLOCK_MONOTONIC: i64 = 1;

fn sys2(n: i64, a: i64, b: i64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") a => ret,
            in("x1") b,
            in("x8") n,
            options(nostack)
        );
    }
    ret
}

fn sys_write(bytes: &[u8]) {
    let ret: i64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") 1_i64 => ret,
            in("x1") bytes.as_ptr() as i64,
            in("x2") bytes.len() as i64,
            in("x8") SYS_WRITE,
            options(nostack)
        );
    }
    let _ = ret;
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
    sys_write(&digits[20 - length..]);
}

fn print(label: &[u8], value: u64) {
    sys_write(label);
    print_u64(value);
}

fn clock_seconds(clock: i64) -> Option<u64> {
    let mut timespec = [0u8; 16];
    if sys2(SYS_CLOCK_GETTIME, clock, timespec.as_mut_ptr() as i64) != 0 {
        return None;
    }
    Some(u64::from_le_bytes({
        let mut seconds = [0u8; 8];
        seconds.copy_from_slice(&timespec[..8]);
        seconds
    }))
}

fn gettimeofday_seconds() -> Option<u64> {
    let mut timeval = [0u8; 16];
    if sys2(SYS_GETTIMEOFDAY, timeval.as_mut_ptr() as i64, 0) != 0 {
        return None;
    }
    Some(u64::from_le_bytes({
        let mut seconds = [0u8; 8];
        seconds.copy_from_slice(&timeval[..8]);
        seconds
    }))
}

#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let realtime = clock_seconds(CLOCK_REALTIME);
    let monotonic = clock_seconds(CLOCK_MONOTONIC);
    let timeofday = gettimeofday_seconds();
    let (Some(realtime), Some(monotonic), Some(timeofday)) = (realtime, monotonic, timeofday)
    else {
        sys_write(b"clock FAILED: syscall error\n");
        sys2(SYS_EXIT, 1, 0);
        loop {}
    };
    print(b"rt=", realtime);
    print(b" mono=", monotonic);
    print(b" gettimeofday=", timeofday);
    sys_write(b"\n");
    // 2020-01-01 .. 2100-01-01: proves the RTC epoch was read, not uptime.
    let epoch_ok = (1_600_000_000..4_000_000_000).contains(&realtime);
    // QEMU test boots finish in seconds; an hour is already generous.
    let monotonic_ok = monotonic < 3_600;
    let timeofday_ok = timeofday.abs_diff(realtime) <= 5;
    let ordered = realtime >= monotonic;
    if epoch_ok && monotonic_ok && timeofday_ok && ordered {
        sys_write(b"clock ok\n");
        sys2(SYS_EXIT, 0, 0);
    } else {
        sys_write(b"clock FAILED\n");
        sys2(SYS_EXIT, 1, 0);
    }
    loop {}
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    sys_write(b"clock panic\n");
    sys2(SYS_EXIT, 1, 0);
    loop {}
}
