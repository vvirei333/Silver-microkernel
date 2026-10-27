#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const UART_PHYS: u64 = 0x0900_0000;
const UART_IRQ: u64 = 33;
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_ICR: usize = 0x44;
const UART_FR_RXFE: u32 = 1 << 4;
const UART_FR_TXFF: u32 = 1 << 5;
/// ICR bits: RXIC (bit 4) and RTIC (bit 6). The receive-timeout flag is raised
/// even when no byte is left in the FIFO, so acknowledging only RXIC leaves the
/// interrupt asserted and the driver spins on it forever.
const UART_RX_RT_CLEAR: u32 = 0x50;
const IPC_INPUT: u8 = 0;
const IPC_OUTPUT: u8 = 1;
const CTRL_Z: u8 = 0x1A;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(target_task_id: u64) -> ! {
    if target_task_id == 0 {
        fail("UART DRIVER INVALID TARGET\r\n");
    }
    let Some(base) = syscall::map_phys(UART_PHYS, 4096) else {
        fail("UART DRIVER MAP FAILED\r\n");
    };
    if !syscall::bind_irq(UART_IRQ) {
        fail("UART DRIVER IRQ BIND FAILED\r\n");
    }

    syscall::print("UART DRIVER STARTED\r\n");
    let base = base as usize;
    loop {
        drain_output(base);
        syscall::wait_irq();
        // Drain the whole FIFO: several bytes can arrive between two wakeups and
        // every one of them has to reach the console owner.
        while read32(base + UART_FR) & UART_FR_RXFE == 0 {
            let byte = read32(base + UART_DR) as u8;
            if byte == CTRL_Z {
                syscall::stop_console();
            } else {
                let message = ipc::Message::new(0, &[IPC_INPUT, byte]);
                let _ = syscall::try_send(target_task_id, &message);
            }
        }
        // Acknowledged unconditionally: an RT interrupt may arrive with no byte
        // left to read, and clearing ICR only inside the read branch would leave
        // it pending. The GIC then re-delivers the same event on every unmask.
        write32(base + UART_ICR, UART_RX_RT_CLEAR);
    }
}

fn drain_output(base: usize) {
    loop {
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive_timeout(&mut message, 0).is_err() {
            return;
        }
        if message.payload[0] != IPC_OUTPUT {
            continue;
        }
        let len = usize::from(message.payload[1]).min(ipc::PAYLOAD_SIZE - 2);
        for &byte in &message.payload[2..2 + len] {
            write_byte(base, byte);
        }
    }
}

fn write_byte(base: usize, byte: u8) {
    while read32(base + UART_FR) & UART_FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    write32(base + UART_DR, u32::from(byte));
}

fn read32(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

fn fail(message: &str) -> ! {
    syscall::print(message);
    syscall::exit_with_status(1)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
