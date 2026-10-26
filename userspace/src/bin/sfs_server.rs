#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[repr(C)]
struct SfsSuperblock {
    magic: u32,
    version: u32,
    block_size: u32,
    total_blocks: u32,
    root_inode: u32,
    inode_count: u32,
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    const BLOCK_DRIVER_TASK_ID: u64 = 3;
    const HOWL_TASK_ID: u64 = 5;
    const SILVERD_TASK_ID: u64 = 1;

    let request = ipc::Message::new(0, &0u64.to_le_bytes());
    let mut superblock = SfsSuperblock { magic: 0, version: 0, block_size: 0, total_blocks: 0, root_inode: 0, inode_count: 0 };
    let mut mounted = false;

    if syscall::send(BLOCK_DRIVER_TASK_ID, &request) {
        let mut response = ipc::Message::new(0, &[]);
        loop {
            if syscall::receive(&mut response) && response.sender == BLOCK_DRIVER_TASK_ID {
                break;
            }
        }
        if response.payload[0] == 1 {
            let mut phys_bytes = [0u8; 8];
            phys_bytes.copy_from_slice(&response.payload[1..9]);
            let phys_addr = u64::from_le_bytes(phys_bytes);

            if let Some(vaddr) = syscall::map_phys(phys_addr, 512) {
                let ack = ipc::Message::new(0, &[0xA5]);
                let _ = syscall::send(BLOCK_DRIVER_TASK_ID, &ack);
                
                superblock = unsafe { core::ptr::read_volatile(vaddr as *const SfsSuperblock) };
                if superblock.magic == 0x53465321 {
                    mounted = true;
                }
            }
        }
    }

    if mounted {
        let ready_msg = ipc::Message::new(0, b"SFS READY");
        let _ = syscall::send(SILVERD_TASK_ID, &ready_msg);
    } else {
        let error_msg = ipc::Message::new(0, b"SFS ERROR");
        let _ = syscall::send(SILVERD_TASK_ID, &error_msg);
    }

    loop {
        let mut request = ipc::Message::new(0, &[]);
        if !syscall::receive(&mut request) || request.sender != HOWL_TASK_ID {
            continue;
        }

        if mounted {
            let mut payload = [0u8; 17];
            payload[0] = 1;
            payload[1..5].copy_from_slice(&superblock.block_size.to_le_bytes());
            payload[5..9].copy_from_slice(&superblock.total_blocks.to_le_bytes());
            let response = ipc::Message::new(0, &payload);
            let _ = syscall::send(HOWL_TASK_ID, &response);
        } else {
            let response = ipc::Message::new(0, &[0]);
            let _ = syscall::send(HOWL_TASK_ID, &response);
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}