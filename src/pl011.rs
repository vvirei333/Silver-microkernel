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
    while unsafe { core::ptr::read_volatile(FR) } & (1 << 5) != 0 {
        core::hint::spin_loop();
    }

    unsafe {
        core::ptr::write_volatile(DR, u32::from(byte));
    }
}

pub fn write_str(message: &str) {
    for &byte in message.as_bytes() {
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
