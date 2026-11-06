//! bigfile: exercises the double-indirect path in SFS.
//!
//! Writes a 72 KiB file (past the 66 KiB single-indirect limit). The content
//! is verified from the host with `scripts/sfs_fsck.py` and a direct scan of
//! the image, because a 512-byte staging buffer on the user stack made the
//! in-process read-back unreliable.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const PATH: &str = "/sfs/big.bin";
const CHUNK: usize = 512;
const CHUNKS: usize = 144; // 72 KiB

/// Static staging buffer: keeps 512 bytes off the small user stack.
static mut BUFFER: [u8; CHUNK] = [0; CHUNK];

fn pattern(chunk: usize, offset: usize) -> u8 {
    ((chunk * 31 + offset) & 0xFF) as u8
}

/// Refills the staging buffer. Kept out of line so the buffer address is
/// re-derived after every syscall instead of living across it in a register.
#[inline(never)]
fn fill(chunk: usize) {
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    for offset in 0..CHUNK {
        buffer[offset] = pattern(chunk, offset);
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };

    if !syscall::fs_create(PATH) {
        syscall::exit_with_status(2);
    }
    let Some(fd) = syscall::fs_open(PATH, 1) else {
        syscall::exit_with_status(2);
    };
    for chunk in 0..CHUNKS {
        fill(chunk);
        match syscall::fs_write(fd, buffer) {
            Some(written) if written == CHUNK => {}
            _ => {
                let _ = syscall::fs_close(fd);
                syscall::exit_with_status(2);
            }
        }
    }
    let _ = syscall::fs_close(fd);
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
