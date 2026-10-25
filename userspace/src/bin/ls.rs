#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const EMPTY: syscall::DirEntry = syscall::DirEntry {
    name: [0; syscall::DIR_NAME_CAPACITY],
    name_len: 0,
    kind: 0,
};

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut arguments = [0_u8; 192];
    let len = syscall::arguments(&mut arguments).unwrap_or(0);
    let path = core::str::from_utf8(&arguments[..len]).unwrap_or("").trim();
    let mut cwd = [0_u8; 192];
    let path = if path.is_empty() {
        let Some(len) = syscall::fs_getcwd(&mut cwd) else {
            syscall::print("ls: cannot read current directory\r\n");
            syscall::exit_with_status(1);
        };
        core::str::from_utf8(&cwd[..len]).unwrap_or("/")
    } else {
        path
    };

    let mut entries = [EMPTY; 32];
    let Some(count) = syscall::fs_read_dir(path, &mut entries) else {
        syscall::print("ls: cannot read directory\r\n");
        syscall::exit_with_status(1);
    };
    for entry in &entries[..count] {
        let Some(name) = entry.name() else {
            continue;
        };
        syscall::print(name);
        if entry.is_directory() {
            syscall::print("/");
        }
        syscall::print("\r\n");
    }
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
