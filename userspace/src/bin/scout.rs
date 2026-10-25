#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../nameserver.rs"]
mod nameserver;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(silverd_id: u64) -> ! {
    if silverd_id != 0 {
        let ready = ipc::Message::new(0, b"SCOUT READY");
        let _ = syscall::send(silverd_id, &ready);
    }
    nameserver::run()
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
