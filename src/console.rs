//! Platform-agnostic console: routes every UART call to the right driver.
//!
//! QEMU `virt` uses a PL011; the tablet uses a Qualcomm GENI SE UART. The rest
//! of the kernel only ever talks to `console::`, so the boot path never has to
//! know which physical device is behind it. The platform is detected once
//! (`platform::detect()`, from `MIDR_EL1`) and cached for the rest of the boot.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::platform::{self, Kind};

const UNCACHED: u8 = 0;
const QEMU: u8 = 1;
const SM8650: u8 = 2;

static KIND: AtomicU8 = AtomicU8::new(UNCACHED);

/// Returns the detected platform, caching the first `MIDR_EL1` read.
fn kind() -> Kind {
    match KIND.load(Ordering::Relaxed) {
        QEMU => Kind::Qemu,
        SM8650 => Kind::Sm8650,
        _ => {
            let detected = platform::detect();
            let tag = if detected.is_tablet() { SM8650 } else { QEMU };
            KIND.store(tag, Ordering::Relaxed);
            detected
        }
    }
}

pub fn init() {
    if kind().is_tablet() {
        crate::geni_uart::init();
    } else {
        crate::pl011::init();
    }
}

pub fn write_byte(byte: u8) {
    if kind().is_tablet() {
        crate::geni_uart::write_byte(byte);
    } else {
        crate::pl011::write_byte(byte);
    }
}

pub fn write_str(message: &str) {
    if kind().is_tablet() {
        crate::geni_uart::write_str(message);
    } else {
        crate::pl011::write_str(message);
    }
}

pub fn write_bytes(bytes: &[u8]) {
    if kind().is_tablet() {
        crate::geni_uart::write_bytes(bytes);
    } else {
        crate::pl011::write_bytes(bytes);
    }
}

pub fn write_hex_u64(value: u64) {
    if kind().is_tablet() {
        crate::geni_uart::write_hex_u64(value);
    } else {
        crate::pl011::write_hex_u64(value);
    }
}

pub fn write_dec_u64(value: u64) {
    if kind().is_tablet() {
        crate::geni_uart::write_dec_u64(value);
    } else {
        crate::pl011::write_dec_u64(value);
    }
}

pub fn read_byte() -> Option<u8> {
    if kind().is_tablet() {
        crate::geni_uart::read_byte()
    } else {
        crate::pl011::read_byte()
    }
}

pub fn handle_irq() {
    if kind().is_tablet() {
        crate::geni_uart::handle_irq();
    } else {
        crate::pl011::handle_irq();
    }
}

pub fn take_stop_request() -> bool {
    if kind().is_tablet() {
        crate::geni_uart::take_stop_request()
    } else {
        crate::pl011::take_stop_request()
    }
}
