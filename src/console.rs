//! Platform-agnostic console: routes every UART call to the right driver.
//!
//! QEMU `virt` uses a PL011; SM8650 and SM6225 use a Qualcomm GENI SE UART.
//! The rest of the kernel only talks to `console::`, so the boot path never
//! has to know which physical device is behind it. The platform is detected
//! once (`platform::detect()`) and the GENI base comes either from the live
//! FDT (`fog::probe`) or from the SM8650 board constant.
//!
//! `Kind::Unknown` deliberately has no output path: writing to an unproven
//! MMIO address is a bus abort, so silence is the only safe behaviour.

use crate::fdt::UartKind;
use crate::platform::{self, Kind};

#[derive(Copy, Clone, Eq, PartialEq)]
enum Active {
    None,
    Pl011,
    Geni,
}

fn active() -> Active {
    match platform::detect() {
        Kind::Qemu => Active::Pl011,
        Kind::Sm8650 => Active::Geni,
        Kind::Fog => {
            let info = crate::fog::info();
            match info.uart_kind {
                UartKind::Geni => Active::Geni,
                // The PL011 driver is QEMU's fixed-base implementation. Only
                // enable it when the live tree names that exact base; a
                // different PL011 belongs to a driver we do not have yet.
                UartKind::Pl011 if info.uart_pa == 0x0900_0000 => Active::Pl011,
                UartKind::Pl011 => Active::None,
            }
        }
        Kind::Unknown => Active::None,
    }
}

pub fn init() {
    match active() {
        Active::Pl011 => crate::pl011::init(),
        Active::Geni => {
            let base = match platform::detect() {
                Kind::Fog => crate::fog::info().uart_pa as usize,
                _ => 0x0089_C000,
            };
            crate::geni_uart::init_at(base);
        }
        Active::None => {}
    }
}

pub fn write_byte(byte: u8) {
    match active() {
        Active::Geni => crate::geni_uart::write_byte(byte),
        Active::Pl011 => crate::pl011::write_byte(byte),
        Active::None => {}
    }
}

pub fn write_str(message: &str) {
    match active() {
        Active::Geni => crate::geni_uart::write_str(message),
        Active::Pl011 => crate::pl011::write_str(message),
        Active::None => {}
    }
}

pub fn write_bytes(bytes: &[u8]) {
    match active() {
        Active::Geni => crate::geni_uart::write_bytes(bytes),
        Active::Pl011 => crate::pl011::write_bytes(bytes),
        Active::None => {}
    }
}

pub fn write_hex_u64(value: u64) {
    match active() {
        Active::Geni => crate::geni_uart::write_hex_u64(value),
        Active::Pl011 => crate::pl011::write_hex_u64(value),
        Active::None => {}
    }
}

pub fn write_dec_u64(value: u64) {
    match active() {
        Active::Geni => crate::geni_uart::write_dec_u64(value),
        Active::Pl011 => crate::pl011::write_dec_u64(value),
        Active::None => {}
    }
}

pub fn read_byte() -> Option<u8> {
    match active() {
        Active::Geni => crate::geni_uart::read_byte(),
        Active::Pl011 => crate::pl011::read_byte(),
        Active::None => None,
    }
}

pub fn handle_irq() {
    match active() {
        Active::Geni => crate::geni_uart::handle_irq(),
        Active::Pl011 => crate::pl011::handle_irq(),
        Active::None => {}
    }
}

pub fn take_stop_request() -> bool {
    match active() {
        Active::Geni => crate::geni_uart::take_stop_request(),
        Active::Pl011 => crate::pl011::take_stop_request(),
        Active::None => false,
    }
}
