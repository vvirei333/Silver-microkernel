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
pub const ROLE_SECURITY_PROBE: u64 = 27;
pub const ROLE_SLEEP_OVERFLOW: u64 = 28;
pub const ROLE_PROCESS_API_PROBE: u64 = 29;
pub const ROLE_PROCESS_API_CHILD: u64 = 30;
pub const ROLE_MOUNT_DENIED_PROBE: u64 = 31;
pub const ROLE_MOUNT_ALLOWED_PROBE: u64 = 32;
pub const ROLE_CONCURRENT_WAITER: u64 = 33;
pub const ROLE_MEMORY_PROBE: u64 = 34;
pub const ROLE_SECRET_WRITER: u64 = 35;
pub const ROLE_SECRET_READER: u64 = 36;
pub const ROLE_POWER_DENIED_PROBE: u64 = 37;
pub const ROLE_POWER_TRUSTED_PROBE: u64 = 38;
pub const ROLE_RESOURCE_PROBE: u64 = 39;
pub const ROLE_MMIO_DMA_PROBE: u64 = 40;
pub const ROLE_DMA_WRITER: u64 = 41;
pub const ROLE_DMA_READER: u64 = 42;
pub const ROLE_ZOMBIE_PROBE: u64 = 43;
pub const ROLE_EXIT_IMMEDIATE: u64 = 44;
pub const ROLE_ELF_LIFECYCLE_PROBE: u64 = 45;

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
        ROLE_SECURITY_PROBE => demos::security_probe(),
        ROLE_SLEEP_OVERFLOW => demos::sleep_overflow(),
        ROLE_PROCESS_API_PROBE => demos::process_api_probe(),
        ROLE_PROCESS_API_CHILD => demos::process_api_child(),
        ROLE_MOUNT_DENIED_PROBE => demos::mount_denied_probe(),
        ROLE_MOUNT_ALLOWED_PROBE => demos::mount_allowed_probe(),
        ROLE_CONCURRENT_WAITER => demos::concurrent_waiter(argument),
        ROLE_MEMORY_PROBE => demos::memory_probe(),
        ROLE_SECRET_WRITER => demos::secret_writer(),
        ROLE_SECRET_READER => demos::secret_reader(),
        ROLE_POWER_DENIED_PROBE => demos::power_denied_probe(),
        ROLE_POWER_TRUSTED_PROBE => demos::power_trusted_probe(),
        ROLE_RESOURCE_PROBE => demos::resource_probe(),
        ROLE_MMIO_DMA_PROBE => demos::mmio_dma_probe(),
        ROLE_DMA_WRITER => demos::dma_writer(),
        ROLE_DMA_READER => demos::dma_reader(),
        ROLE_ZOMBIE_PROBE => demos::zombie_probe(),
        ROLE_EXIT_IMMEDIATE => demos::exit_immediate(),
        ROLE_ELF_LIFECYCLE_PROBE => demos::elf_lifecycle_probe(),
        _ => demos::unknown_task(),
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
