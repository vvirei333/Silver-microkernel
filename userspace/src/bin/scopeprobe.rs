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
    let Some(before) = syscall::memory_info_v2() else {
        syscall::exit_with_status(2);
    };
    let Some(health_before) = syscall::health_info() else {
        syscall::exit_with_status(2);
    };

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

    // Atomic replacement: f1 must end up with f0's bytes and f0 must vanish.
    let replaced_ok = write_file("/sfs/probe-scope/f0", b"AAA")
        && write_file("/sfs/probe-scope/f1", b"BBBB")
        && syscall::fs_replace("/sfs/probe-scope/f0", "/sfs/probe-scope/f1")
        && read_matches("/sfs/probe-scope/f1", b"AAA")
        && syscall::fs_open("/sfs/probe-scope/f0", 0).is_none();

    for path in paths {
        let _ = syscall::fs_unlink(path);
    }

    let leaks_ok = match syscall::memory_info_v2() {
        Some(after) => {
            after.open_files == 0
                && after.dma_pages == before.dma_pages
                && after.task_frames == before.task_frames
        }
        None => false,
    } && match syscall::health_info() {
        Some(health_after) => {
            health_after.dma_leaked_pages == health_before.dma_leaked_pages
                && health_after.dma_quarantined_pages == health_before.dma_quarantined_pages
                && health_after.open_files == health_before.open_files
        }
        None => false,
    };

    if inside && !outside && !escape && quota_ok && created == 4 && replaced_ok && leaks_ok {
        syscall::exit();
    }
    syscall::exit_with_status(3);
}

fn write_file(path: &str, contents: &[u8]) -> bool {
    if !syscall::fs_create(path) {
        return false;
    }
    let Some(fd) = syscall::fs_open(path, 1) else {
        return false;
    };
    let ok = syscall::fs_write(fd, contents) == Some(contents.len());
    let _ = syscall::fs_close(fd);
    ok
}

fn read_matches(path: &str, expected: &[u8]) -> bool {
    let Some(fd) = syscall::fs_open(path, 0) else {
        return false;
    };
    let mut buffer = [0_u8; 16];
    let read = syscall::fs_read(fd, &mut buffer);
    let _ = syscall::fs_close(fd);
    matches!(read, Some(len) if len == expected.len() && &buffer[..len] == expected)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
