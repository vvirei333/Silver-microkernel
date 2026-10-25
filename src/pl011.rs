use core::sync::atomic::{AtomicBool, Ordering};

const BASE: usize = crate::memory::phys_to_virt(0x0900_0000);
const DR: *mut u32 = BASE as *mut u32;
const FR: *const u32 = (BASE + 0x18) as *const u32;
const IBRD: *mut u32 = (BASE + 0x24) as *mut u32;
const FBRD: *mut u32 = (BASE + 0x28) as *mut u32;
const LCR_H: *mut u32 = (BASE + 0x2C) as *mut u32;
const CR: *mut u32 = (BASE + 0x30) as *mut u32;
const IFLS: *mut u32 = (BASE + 0x34) as *mut u32;
const IMSC: *mut u32 = (BASE + 0x38) as *mut u32;
const ICR: *mut u32 = (BASE + 0x44) as *mut u32;

const CTRL_Z: u8 = 0x1A;
const RX_INTERRUPT: u32 = 1 << 4;
const RX_TIMEOUT_INTERRUPT: u32 = 1 << 5;
const RX_CAPACITY: usize = 256;
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static mut RX_QUEUE: [u8; RX_CAPACITY] = [0; RX_CAPACITY];
static mut RX_HEAD: usize = 0;
static mut RX_LEN: usize = 0;

pub fn init() {
    unsafe {
        core::ptr::write_volatile(CR, 0);
        core::ptr::write_volatile(IMSC, 0);
        core::ptr::write_volatile(ICR, RX_INTERRUPT | RX_TIMEOUT_INTERRUPT);
        core::ptr::write_volatile(IBRD, 13);
        core::ptr::write_volatile(FBRD, 1);
        core::ptr::write_volatile(LCR_H, (1 << 4) | (0b11 << 5));
        // RX at 1/8 FIFO full; RTIM makes a single pending byte arrive without
        // waiting for the threshold. Clear stale interrupts before unmasking.
        core::ptr::write_volatile(IFLS, 0);
        core::ptr::write_volatile(ICR, RX_INTERRUPT | RX_TIMEOUT_INTERRUPT);
        core::ptr::write_volatile(CR, 1 | (1 << 8) | (1 << 9));
        core::ptr::write_volatile(IMSC, RX_INTERRUPT | RX_TIMEOUT_INTERRUPT);
    }
}

/// PL011 IRQ path: drain every available hardware byte into the bounded
/// software FIFO. Ctrl-Z is consumed as a job-control request, not input.
pub fn handle_irq() {
    let saved_daif = crate::interrupts::save_and_disable_irq();
    while let Some(byte) = read_hardware() {
        if byte == CTRL_Z {
            STOP_REQUESTED.store(true, Ordering::Relaxed);
        } else {
            push_rx(byte);
        }
    }
    unsafe {
        core::ptr::write_volatile(ICR, RX_INTERRUPT | RX_TIMEOUT_INTERRUPT);
    }
    crate::interrupts::restore_irq(saved_daif);
}

pub fn take_stop_request() -> bool {
    STOP_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Returns one byte from the IRQ-filled queue without touching hardware.
pub fn read_byte() -> Option<u8> {
    let saved_daif = crate::interrupts::save_and_disable_irq();
    let result = pop_rx();
    crate::interrupts::restore_irq(saved_daif);
    result
}

fn read_hardware() -> Option<u8> {
    // Bit 4 of FR is RXFE (RX FIFO empty).
    if unsafe { core::ptr::read_volatile(FR) } & (1 << 4) != 0 {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(DR) } as u8)
}

fn push_rx(byte: u8) {
    unsafe {
        let len = core::ptr::addr_of_mut!(RX_LEN).read();
        if len == RX_CAPACITY {
            return;
        }
        let head = core::ptr::addr_of_mut!(RX_HEAD).read();
        let tail = (head + len) % RX_CAPACITY;
        core::ptr::addr_of_mut!(RX_QUEUE)
            .cast::<u8>()
            .add(tail)
            .write(byte);
        core::ptr::addr_of_mut!(RX_LEN).write(len + 1);
    }
}

fn pop_rx() -> Option<u8> {
    unsafe {
        let len = core::ptr::addr_of_mut!(RX_LEN).read();
        if len == 0 {
            return None;
        }
        let head = core::ptr::addr_of_mut!(RX_HEAD).read();
        let byte = core::ptr::addr_of!(RX_QUEUE).cast::<u8>().add(head).read();
        core::ptr::addr_of_mut!(RX_HEAD).write((head + 1) % RX_CAPACITY);
        core::ptr::addr_of_mut!(RX_LEN).write(len - 1);
        Some(byte)
    }
}

pub fn write_byte(byte: u8) {
    // Wait for TX FIFO space with IRQs unmasked — but restore the caller's
    // DAIF afterwards so this stays transparent in any context (syscall
    // handlers, the scheduler's BAD SP guard, panic dumps). Spinning on TXFF
    // with the interrupt flag set would freeze the whole system: the guest
    // prints in bursts far faster than the host drains the UART, and a
    // masked timer can never preempt a spinning syscall handler. The
    // trampolines keep ELR/SPSR in their stack frames, so taking an IRQ
    // mid-print is safe — the handler returns exactly here.
    if unsafe { core::ptr::read_volatile(FR) } & (1 << 5) != 0 {
        let saved_daif = crate::interrupts::save_and_disable_irq();
        while unsafe { core::ptr::read_volatile(FR) } & (1 << 5) != 0 {
            crate::interrupts::enable_irq();
            core::hint::spin_loop();
        }
        crate::interrupts::restore_irq(saved_daif);
    }

    unsafe {
        core::ptr::write_volatile(DR, u32::from(byte));
    }
}

pub fn write_str(message: &str) {
    write_bytes(message.as_bytes());
}

pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}

pub fn write_hex_u64(value: u64) {
    for shift in (0..64).step_by(4).rev() {
        let digit = ((value >> shift) & 0xF) as u8;
        let byte = match digit {
            0x0..=0x9 => b'0' + digit,
            _ => b'A' + digit - 0xA,
        };
        write_byte(byte);
    }
}

pub fn write_dec_u64(value: u64) {
    let mut digits = [0_u8; 20];
    let mut count = 0;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
        if rest == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        write_byte(digits[count]);
    }
}
