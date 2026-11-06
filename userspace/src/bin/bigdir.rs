//! bigdir: filesystem stress helper.
//!
//! Creates a large batch of files in one directory, so the directory-growth
//! path in SFS (four direct blocks, then a single indirect table) and the
//! inode allocator can be exercised from the shell:
//!
//!   run /bin/bigdir
//!
//! The process exits with status 0 when every operation succeeded.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const COUNT: u32 = 300;

fn file_path(index: u32) -> [u8; 9] {
    let mut path = *b"/sfs/g000";
    path[6] = b'0' + (index / 100) as u8;
    path[7] = b'0' + ((index / 10) % 10) as u8;
    path[8] = b'0' + (index % 10) as u8;
    path
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut failures = 0u32;
    for index in 0..COUNT {
        let path = file_path(index);
        let Ok(text) = core::str::from_utf8(&path) else {
            failures += 1;
            continue;
        };
        if !syscall::fs_create(text) {
            failures += 1;
        }
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
