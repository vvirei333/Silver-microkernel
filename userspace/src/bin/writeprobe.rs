//! writeprobe: verifies that an EL0 program launched from a file has no
//! filesystem write capability.
//!
//! Run it from the shell with `run /bin/writeprobe`. Exit status:
//!   0 — the write was denied (expected: dynamic ELFs get no CAP_FS_WRITE)
//!   2 — the write succeeded (a sandbox hole)

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    if syscall::fs_create("/sfs/writeprobe") {
        let _ = syscall::fs_unlink("/sfs/writeprobe");
        syscall::exit_with_status(2);
    }
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
