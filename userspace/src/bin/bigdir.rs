//! bigdir: filesystem stress helper for directory growth and shrink.
//!
//!   bigdir        create /sfs/d and 300 files inside it
//!   bigdir rm     remove those files, then rmdir /sfs/d
//!
//! The grow path exercises the directory indirect table; the remove path
//! exercises per-block compaction. Exit status 0 means every operation
//! succeeded.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const COUNT: u32 = 300;
const DIR: &str = "/sfs/d";

fn file_path(index: u32) -> [u8; 11] {
    let mut path = *b"/sfs/d/g000";
    path[7] = b'0' + (index / 100) as u8;
    path[8] = b'0' + ((index / 10) % 10) as u8;
    path[9] = b'0' + (index % 10) as u8;
    path
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut arguments = [0_u8; 32];
    let len = syscall::arguments(&mut arguments).unwrap_or(0);
    let remove = core::str::from_utf8(&arguments[..len])
        .unwrap_or("")
        .trim()
        == "rm";

    let mut failures = 0u32;
    if !remove && !syscall::fs_mkdir(DIR) {
        // An existing directory is fine; a real failure shows up below.
    }

    for index in 0..COUNT {
        let path = file_path(index);
        let Ok(text) = core::str::from_utf8(&path) else {
            failures += 1;
            continue;
        };
        let ok = if remove {
            syscall::fs_unlink(text)
        } else {
            syscall::fs_create(text)
        };
        if !ok {
            failures += 1;
        }
    }

    if remove && !syscall::fs_rmdir(DIR) {
        failures += 1;
    }

    if failures != 0 {
        syscall::exit_with_status(1);
    }
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
