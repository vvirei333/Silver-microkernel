//! scopeprobe: verifies a bundle manifest's path-scoped write grant.
//!
//! It is installed as `/sfs/apps/scopeprobe.elf` next to a manifest that says
//! `write=/sfs/probe-scope`, so the kernel should let it write *inside* that
//! prefix and deny everything else. Exit status:
//!   0 — inside allowed, outside denied (expected)
//!   2 — the scope was not enforced correctly

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
    let _ = syscall::fs_mkdir("/sfs/probe-scope");

    let inside = syscall::fs_create("/sfs/probe-scope/ok");
    if inside {
        let _ = syscall::fs_unlink("/sfs/probe-scope/ok");
    }

    let outside = syscall::fs_create("/sfs/probe-outside");
    if outside {
        let _ = syscall::fs_unlink("/sfs/probe-outside");
    }

    if inside && !outside {
        syscall::exit();
    }
    syscall::exit_with_status(2);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
