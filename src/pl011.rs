const BASE: usize = 0x0900_0000;
const DR: *mut u32 = BASE as *mut u32;
const FR: *const u32 = (BASE + 0x18) as *const u32;
const IBRD: *mut u32 = (BASE + 0x24) as *mut u32;
const FBRD: *mut u32 = (BASE + 0x28) as *mut u32;
const LCR_H: *mut u32 = (BASE + 0x2C) as *mut u32;
const CR: *mut u32 = (BASE + 0x30) as *mut u32;

pub fn init() {
    unsafe {
        core::ptr::write_volatile(CR, 0);
        core::ptr::write_volatile(IBRD, 13);
        core::ptr::write_volatile(FBRD, 1);
        core::ptr::write_volatile(LCR_H, (1 << 4) | (0b11 << 5));
        core::ptr::write_volatile(CR, 1 | (1 << 8) | (1 << 9));
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
