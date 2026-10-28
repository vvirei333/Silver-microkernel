//! Intentional-crash test program for EL0 fault containment.
//!
//! Writes to the unmapped address `0x0`, which must raise an EL0 data abort.
//! The kernel is expected to kill only this task (logging a "Page Fault" line)
//! and return control to the shell. Seeded into the VFS like `hello.elf` and
//! run with `run bin/crash` — deliberately NOT in `user_program::image`.

#![no_std]
#![no_main]

#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("crash: writing to 0x0...\r\n");
    // `write_volatile` guarantees the store is emitted even in release builds.
    unsafe {
        core::ptr::write_volatile(0x0 as *mut u64, 42);
    }
    // Unreachable if the fault handler kills this task first.
    syscall::print("crash: SURVIVED (BUG)\r\n");
    loop {
        syscall::sleep(1000);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
