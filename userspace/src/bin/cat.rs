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
    let mut arguments = [0_u8; 192];
    let len = syscall::arguments(&mut arguments).unwrap_or(0);
    let path = core::str::from_utf8(&arguments[..len]).unwrap_or("").trim();
    if path.is_empty() {
        syscall::print("Usage: cat <file>\r\n");
        syscall::exit_with_status(1);
    }

    let Some(fd) = syscall::fs_open(path, syscall::OPEN_READ) else {
        syscall::print("cat: cannot open file\r\n");
        syscall::exit_with_status(1);
    };
    let mut buffer = [0_u8; 256];
    loop {
        match syscall::fs_read(fd, &mut buffer) {
            Some(0) => break,
            Some(count) => syscall::print_bytes(&buffer[..count]),
            None => {
                syscall::fs_close(fd);
                syscall::print("cat: read failed\r\n");
                syscall::exit_with_status(1);
            }
        }
    }
    syscall::fs_close(fd);
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
