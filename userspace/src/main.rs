#![no_std]
#![no_main]

mod demos;
mod ipc;
mod nameserver;
mod syscall;

use core::panic::PanicInfo;

/// One separately linked ELF contains all demo roles. The kernel loads an
/// independent copy for each task; task_id selects the role at runtime.
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    match syscall::task_id() {
        1 => nameserver::run(),
        2 => demos::mutex_holder(),
        3 | 4 => demos::mutex_contender(),
        5 => demos::ipc_ping(),
        6 => demos::ipc_pong(),
        7 => demos::condvar_alpha(),
        8 => demos::condvar_beta(),
        9 => demos::condvar_probe(),
        10 => demos::channel_producer(),
        11 => demos::channel_consumer(),
        12 => demos::sleeping_task(),
        13 => demos::vm_task_a(),
        14 => demos::vm_task_b(),
        15 => demos::isolation_probe(),
        16 => demos::kernel_image_probe(),
        _ => demos::unknown_task(),
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
