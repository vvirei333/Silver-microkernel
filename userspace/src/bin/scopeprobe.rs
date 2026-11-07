//! scopeprobe: verifies a bundle manifest's path-scoped write grant.
//!
//! It is installed as `/sfs/apps/scopeprobe.elf` next to a manifest that says
//! `write=/sfs/probe-scope` and `quota=8:5`, so the kernel should let it write
//! *inside* that prefix, deny everything else (including `..` escapes) and
//! stop it after four files. Exit status:
//!   0 — scope, traversal and quota all enforced (expected)
//!   2 — one of the checks was not enforced correctly

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

    // The raw string starts with the allowed prefix, but resolves outside it.
    // A correct kernel checks the resolved path.
    let escape = syscall::fs_create("/sfs/probe-scope/../probe-escape");
    if escape {
        let _ = syscall::fs_unlink("/sfs/probe-escape");
    }

    // quota=8:5 leaves room for the directory plus four files (five inodes).
    // The fifth file must be refused.
    let mut created = 0u32;
    let mut quota_ok = true;
    let paths = [
        "/sfs/probe-scope/f0",
        "/sfs/probe-scope/f1",
        "/sfs/probe-scope/f2",
        "/sfs/probe-scope/f3",
    ];
    for path in paths {
        if syscall::fs_create(path) {
            created += 1;
        } else {
            quota_ok = false;
        }
    }
    if syscall::fs_create("/sfs/probe-scope/f4") {
        quota_ok = false;
    }
    for path in paths {
        let _ = syscall::fs_unlink(path);
    }

    if inside && !outside && !escape && quota_ok && created == 4 {
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
