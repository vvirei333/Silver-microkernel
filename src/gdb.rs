//! Minimal GDB remote-serial-protocol stub on the second PL011 (0x0904_0000).
//!
//! QEMU `virt` exposes four PL011s; the console owns the first one, so the
//! debugger gets its own wire and the two never interleave. The stub is
//! read-mostly on purpose: a user fault is already final, so `g`, `m` and `M`
//! are enough to inspect the dying address space, and `c`/`D` release the task
//! to the normal kill path. There is no `s`/breakpoint support yet.
//!
//! Everything here runs in exception context: no allocation, no locks, no
//! console output. Buffers are fixed-size and the UART is polled.

use crate::memory::phys_to_virt;

const UART_BASE: usize = phys_to_virt(0x0904_0000);
const DR: *mut u32 = UART_BASE as *mut u32;
const FR: *const u32 = (UART_BASE + 0x18) as *const u32;
const IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const LCR_H: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;
const IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;

const PACKET_MAX: usize = 2048;

const TARGET_XML: &str = "<?xml version=\"1.0\"?>\
<!DOCTYPE target SYSTEM \"gdb-target.dtd\">\
<target><architecture>aarch64</architecture><feature name=\"org.gnu.gdb.aarch64.core\">\
<reg name=\"x0\" bitsize=\"64\"/><reg name=\"x1\" bitsize=\"64\"/><reg name=\"x2\" bitsize=\"64\"/>\
<reg name=\"x3\" bitsize=\"64\"/><reg name=\"x4\" bitsize=\"64\"/><reg name=\"x5\" bitsize=\"64\"/>\
<reg name=\"x6\" bitsize=\"64\"/><reg name=\"x7\" bitsize=\"64\"/><reg name=\"x8\" bitsize=\"64\"/>\
<reg name=\"x9\" bitsize=\"64\"/><reg name=\"x10\" bitsize=\"64\"/><reg name=\"x11\" bitsize=\"64\"/>\
<reg name=\"x12\" bitsize=\"64\"/><reg name=\"x13\" bitsize=\"64\"/><reg name=\"x14\" bitsize=\"64\"/>\
<reg name=\"x15\" bitsize=\"64\"/><reg name=\"x16\" bitsize=\"64\"/><reg name=\"x17\" bitsize=\"64\"/>\
<reg name=\"x18\" bitsize=\"64\"/><reg name=\"x19\" bitsize=\"64\"/><reg name=\"x20\" bitsize=\"64\"/>\
<reg name=\"x21\" bitsize=\"64\"/><reg name=\"x22\" bitsize=\"64\"/><reg name=\"x23\" bitsize=\"64\"/>\
<reg name=\"x24\" bitsize=\"64\"/><reg name=\"x25\" bitsize=\"64\"/><reg name=\"x26\" bitsize=\"64\"/>\
<reg name=\"x27\" bitsize=\"64\"/><reg name=\"x28\" bitsize=\"64\"/><reg name=\"x29\" bitsize=\"64\"/>\
<reg name=\"x30\" bitsize=\"64\"/><reg name=\"sp\" bitsize=\"64\" type=\"data_ptr\"/>\
<reg name=\"pc\" bitsize=\"64\" type=\"code_ptr\"/><reg name=\"cpsr\" bitsize=\"32\"/>\
</feature></target>";

fn initialize() {
    unsafe {
        core::ptr::write_volatile(CR, 0);
        core::ptr::write_volatile(IMSC, 0);
        core::ptr::write_volatile(ICR, 0x7ff);
        core::ptr::write_volatile(IBRD, 13);
        core::ptr::write_volatile(FBRD, 1);
        core::ptr::write_volatile(LCR_H, (1 << 4) | (0b11 << 5));
        core::ptr::write_volatile(CR, 1 | (1 << 8) | (1 << 9));
    }
}

fn write_byte(byte: u8) {
    while unsafe { core::ptr::read_volatile(FR) } & (1 << 5) != 0 {
        core::hint::spin_loop();
    }
    unsafe { core::ptr::write_volatile(DR, byte as u32) };
}

fn read_byte() -> u8 {
    loop {
        if unsafe { core::ptr::read_volatile(FR) } & (1 << 4) == 0 {
            return unsafe { core::ptr::read_volatile(DR) } as u8;
        }
        core::hint::spin_loop();
    }
}

fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value - 10,
    }
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex(bytes: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for byte in bytes {
        value = value.checked_mul(16)?.checked_add(from_hex(*byte)? as u64)?;
    }
    Some(value)
}

fn send_packet(payload: &[u8]) {
    let mut checksum = 0u8;
    write_byte(b'$');
    for byte in payload {
        checksum = checksum.wrapping_add(*byte);
        write_byte(*byte);
    }
    write_byte(b'#');
    write_byte(hex_digit(checksum >> 4));
    write_byte(hex_digit(checksum & 0xf));
}

fn recv_packet(buffer: &mut [u8]) -> usize {
    // Wait for '$'; ignore acks and anything else on the wire.
    loop {
        if read_byte() == b'$' {
            break;
        }
    }
    let mut length = 0usize;
    loop {
        let byte = read_byte();
        if byte == b'#' {
            break;
        }
        if length < buffer.len() {
            buffer[length] = byte;
            length += 1;
        }
    }
    let _high = from_hex(read_byte()).unwrap_or(0);
    let _low = from_hex(read_byte()).unwrap_or(0);
    write_byte(b'+');
    length
}

fn append_hex(buffer: &mut [u8], length: &mut usize, value: u64, bytes: usize) {
    for index in (0..bytes).rev() {
        if *length + 2 <= buffer.len() {
            let byte = (value >> (index * 8)) as u8;
            buffer[*length] = hex_digit(byte >> 4);
            buffer[*length + 1] = hex_digit(byte & 0xf);
            *length += 2;
        }
    }
}

fn read_user_u64(address: u64) -> Option<u64> {
    // Only the task's own, non-kernel address space. A bad address here would
    // fault in EL1 (fatal), so keep the range check conservative.
    if address == 0 || address >= 0x0000_8000_0000_0000 {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(address as *const u64) })
}

fn copy_user(address: u64, out: &mut [u8]) -> bool {
    if address == 0 || address >= 0x0000_8000_0000_0000 {
        return false;
    }
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile((address as usize + index) as *const u8) };
    }
    true
}

fn write_user(address: u64, bytes: &[u8]) -> bool {
    if address == 0 || address >= 0x0000_8000_0000_0000 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        unsafe { core::ptr::write_volatile((address as usize + index) as *mut u8, *byte) };
    }
    true
}

/// Serves the debugger after a user fault. Returns when the debugger detaches
/// or continues, so the caller can run its normal kill path.
pub fn serve(frame: *const u64, _esr: u64, elr: u64, _far: u64) {
    // This QEMU build's `virt` machine exposes a single PL011 in its device
    // tree (verified by dumping the DTB); the second UART this stub was
    // written against (0x0904_0000) does not exist, and every MMIO touch
    // raises a synchronous external abort that turns a user fault into a
    // kernel panic. The software page watch in log mode prints its evidence
    // to the console, so the stub stays dormant until it gains a real
    // transport (QEMU's own `-gdb` gdbstub or a virtio-serial console).
    let _ = (frame, elr);
    return;
    #[allow(unreachable_code)]
    initialize();
    for byte in b"\nGDB-STUB aarch64\n" {
        write_byte(*byte);
    }

    let mut packet = [0u8; PACKET_MAX];
    let mut reply = [0u8; PACKET_MAX];
    loop {
        let length = recv_packet(&mut packet);
        let command = &packet[..length];
        let mut reply_length = 0usize;

        if command == b"?" {
            reply[..3].copy_from_slice(b"S05");
            reply_length = 3;
        } else if command == b"g" {
            // 31 general registers, sp, pc, cpsr (aarch64 g-packet layout).
            for index in 0..31 {
                let value = unsafe { *frame.add(index) };
                append_hex(&mut reply, &mut reply_length, value, 8);
            }
            let sp: u64;
            unsafe {
                core::arch::asm!("mrs {}, sp_el0", out(reg) sp);
            }
            append_hex(&mut reply, &mut reply_length, sp, 8);
            append_hex(&mut reply, &mut reply_length, elr, 8);
            append_hex(&mut reply, &mut reply_length, 0, 4);
        } else if command == b"qSupported" || command.starts_with(b"qSupported:") {
            let text = b"PacketSize=800;qXfer:features:read+";
            reply[..text.len()].copy_from_slice(text);
            reply_length = text.len();
        } else if command.starts_with(b"qXfer:features:read:target.xml:") {
            // "<prefix>offset,length"
            let rest = &command[b"qXfer:features:read:target.xml:".len()..];
            let mut parts = rest.split(|byte| *byte == b',');
            let offset = parts.next().and_then(|part| parse_hex(part)).unwrap_or(0);
            let count = parts.next().and_then(|part| parse_hex(part)).unwrap_or(0) as usize;
            let xml = TARGET_XML.as_bytes();
            if offset as usize >= xml.len() {
                reply[0] = b'l';
                reply_length = 1;
            } else {
                let end = (offset as usize + count).min(xml.len());
                let chunk = &xml[offset as usize..end];
                reply[0] = if end == xml.len() { b'l' } else { b'm' };
                reply[1..1 + chunk.len()].copy_from_slice(chunk);
                reply_length = 1 + chunk.len();
            }
        } else if command == b"qAttached" {
            reply[0] = b'1';
            reply_length = 1;
        } else if command.starts_with(b"m") {
            // m<addr>,<length>
            let rest = &command[1..];
            let mut parts = rest.split(|byte| *byte == b',');
            let address = parts.next().and_then(|part| parse_hex(part));
            let count = parts.next().and_then(|part| parse_hex(part));
            match (address, count) {
                (Some(address), Some(count)) if count <= (PACKET_MAX as u64) / 2 => {
                    let mut bytes = [0u8; PACKET_MAX / 2];
                    let length = count as usize;
                    if copy_user(address, &mut bytes[..length]) {
                        for index in 0..length {
                            reply[reply_length] = hex_digit(bytes[index] >> 4);
                            reply[reply_length + 1] = hex_digit(bytes[index] & 0xf);
                            reply_length += 2;
                        }
                    } else {
                        reply[0] = b'E';
                        reply[1] = b'0';
                        reply[2] = b'1';
                        reply_length = 3;
                    }
                }
                _ => {
                    reply[0] = b'E';
                    reply[1] = b'0';
                    reply[2] = b'2';
                    reply_length = 3;
                }
            }
        } else if command.starts_with(b"M") {
            // M<addr>,<length>:<hex bytes>
            let rest = &command[1..];
            if let Some(colon) = rest.iter().position(|byte| *byte == b':') {
                let header = &rest[..colon];
                let body = &rest[colon + 1..];
                let mut parts = header.split(|byte| *byte == b',');
                let address = parts.next().and_then(|part| parse_hex(part));
                let count = parts.next().and_then(|part| parse_hex(part));
                let mut bytes = [0u8; PACKET_MAX / 2];
                let mut valid = body.len() % 2 == 0;
                let length = body.len() / 2;
                if length <= bytes.len() {
                    for index in 0..length {
                        let high = from_hex(body[index * 2]);
                        let low = from_hex(body[index * 2 + 1]);
                        match (high, low) {
                            (Some(high), Some(low)) => bytes[index] = (high << 4) | low,
                            _ => valid = false,
                        }
                    }
                } else {
                    valid = false;
                }
                if valid
                    && count == Some(length as u64)
                    && address.map(|address| write_user(address, &bytes[..length])) == Some(true)
                {
                    reply[..2].copy_from_slice(b"OK");
                    reply_length = 2;
                } else {
                    reply[..3].copy_from_slice(b"E03");
                    reply_length = 3;
                }
            } else {
                reply[..3].copy_from_slice(b"E04");
                reply_length = 3;
            }
        } else if command == b"c" || command == b"s" || command == b"D" || command == b"k" {
            // The fault is final: release the task to the normal kill path.
            if command == b"D" {
                reply[..2].copy_from_slice(b"OK");
                reply_length = 2;
            } else {
                reply[..3].copy_from_slice(b"W00");
                reply_length = 3;
            }
            send_packet(&reply[..reply_length]);
            return;
        } else {
            reply_length = 0;
        }

        send_packet(&reply[..reply_length]);
    }
}
