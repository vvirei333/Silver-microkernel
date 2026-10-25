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
    syscall::print("BLOCK DRIVER STARTED\r\n");

    let Some((virtual_address, _physical_address)) = syscall::alloc_dma(4 * 4096) else {
        syscall::exit_with_status(1);
    };
    unsafe {
        core::ptr::write_volatile(virtual_address as *mut u32, 0x5349_4C56);
        if core::ptr::read_volatile(virtual_address as *const u32) != 0x5349_4C56 {
            syscall::exit_with_status(1);
        }
    }
    if syscall::pci_read32(0).is_none() {
        syscall::exit_with_status(1);
    }

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
