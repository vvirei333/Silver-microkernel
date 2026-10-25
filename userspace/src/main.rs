#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod demos;
mod ipc;
mod syscall;

use core::panic::PanicInfo;

#[global_allocator]
static GLOBAL_ALLOCATOR: allocator::UserAllocator = allocator::UserAllocator;

pub const ROLE_LIFECYCLE_SUPERVISOR: u64 = 17;
pub const ROLE_LIFECYCLE_CHILD: u64 = 18;
pub const ROLE_COPYOUT_PROBE: u64 = 19;
pub const ROLE_HEAP_PROBE: u64 = 20;
pub const ROLE_MALLOC_PROBE: u64 = 21;
pub const ROLE_GLOBAL_ALLOC_PROBE: u64 = 22;
pub const ROLE_IPC_BACKPRESSURE_SENDER: u64 = 23;
pub const ROLE_IPC_BACKPRESSURE_RECEIVER: u64 = 24;
pub const ROLE_IPC_EXIT_SENDER: u64 = 25;
pub const ROLE_IPC_EXIT_RECEIVER: u64 = 26;

/// The kernel passes one argument in x0. Demo selection no longer depends on
/// task ID: dynamically spawned copies can choose any role independently.
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(argument: u64) -> ! {
    match argument {
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
        ROLE_LIFECYCLE_SUPERVISOR => demos::lifecycle_supervisor(),
        ROLE_LIFECYCLE_CHILD => demos::lifecycle_child(),
        ROLE_COPYOUT_PROBE => demos::copyout_probe(),
        ROLE_HEAP_PROBE => demos::heap_probe(),
        ROLE_MALLOC_PROBE => demos::malloc_probe(),
        ROLE_GLOBAL_ALLOC_PROBE => demos::global_alloc_probe(),
        ROLE_IPC_BACKPRESSURE_SENDER => demos::ipc_backpressure_sender(),
        ROLE_IPC_BACKPRESSURE_RECEIVER => demos::ipc_backpressure_receiver(),
        ROLE_IPC_EXIT_SENDER => demos::ipc_exit_sender(),
        ROLE_IPC_EXIT_RECEIVER => demos::ipc_exit_receiver(),
        _ => demos::unknown_task(),
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
