//! Minimal Ring 3 program used to exercise the dynamic ELF loader (Phase 2).
//!
//! This program is intentionally NOT embedded into the kernel via
//! `include_bytes!`. It builds alongside the other userspace binaries as a
//! plain AArch64 ELF64 (`aarch64-unknown-none`), and is meant to be copied
//! into SFS and started with a future `run bin/hello` shell command that loads
//! the ELF from the filesystem instead of the embedded image table.

#![no_std]
#![no_main]

#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("Hello from ELF!\r\n");
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
