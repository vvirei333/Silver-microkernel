//! Arms the software page watch (log mode) before the Alpine session starts.
//!
//! The phoc corruption lands on a heap page that is stable across runs
//! (`0x44A41000`; observed `PhocXdgToplevel` objects at 0x44A41E40/0x44A41EA0).
//! The Alpine `boot-session` runs this binary first, so every later mapping of
//! that page is created write-protected and the kernel logs the store address
//! and PC of each write (`WATCH* far=... elr=...`) instead of killing the task.
#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SYS_DEBUG_WATCH: u64 = 92;
/// The watched page (must match the observed phoc heap placement).
const WATCH_PAGE: u64 = 0x44A4_1000;
const WATCH_LENGTH: u64 = 4096;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let accepted = unsafe { syscall2(SYS_DEBUG_WATCH, WATCH_PAGE, WATCH_LENGTH) };
    if accepted == 1 {
        syscall::print("armwatch: page watch armed\r\n");
    } else {
        syscall::print("armwatch: FAILED\r\n");
    }
    syscall::exit();
}

// Direct raw syscall (userspace syscall.rs has no 2-argument wrapper public).
unsafe fn syscall2(number: u64, a0: u64, a1: u64) -> i64 {
    let result;
    core::arch::asm!(
        "svc #0",
        inlateout("x8") number => _,
        inlateout("x0") a0 => result,
        inlateout("x1") a1 => _,
        lateout("x2") _,
        lateout("x3") _,
        lateout("x4") _,
        lateout("x5") _,
        lateout("x6") _,
        lateout("x7") _,
        lateout("x16") _,
        lateout("x17") _,
    );
    result
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        syscall::sleep(1000);
    }
}
