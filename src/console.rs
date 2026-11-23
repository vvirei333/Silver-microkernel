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

use core::sync::atomic::{AtomicBool, Ordering};

use crate::fdt::UartKind;
use crate::platform::{self, Kind};

/// Best-effort writer serialization for the single UART FIFO.
///
/// Every CPU shares one transmitter; without this, two PEs interleave bytes
/// *inside* one `write_str` and the boot log becomes unreadable (visible as
/// soon as three APs print on QEMU `-smp 4`). This is deliberately not a hard
/// spinlock: `pl011::write_byte` re-enables IRQs while it waits for FIFO space,
/// so the owner can be preempted and the lock must never be held across a
/// context switch by contract. A bounded spin serializes the common short
/// write; on timeout the caller writes anyway, because interleaved diagnostics
/// beat a silent hang (this path is also used by the panic dumper).
static WRITER_BUSY: AtomicBool = AtomicBool::new(false);
const WRITER_SPIN_LIMIT: u32 = 100_000;

struct WriterGuard {
    owned: bool,
}

impl WriterGuard {
    fn acquire() -> Self {
        let mut spins = 0u32;
        loop {
            if WRITER_BUSY
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Self { owned: true };
            }
            spins += 1;
            if spins >= WRITER_SPIN_LIMIT {
                return Self { owned: false };
            }
            unsafe {
                core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
            }
        }
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if self.owned {
            WRITER_BUSY.store(false, Ordering::Release);
        }
    }
}

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

/// One console line assembled in a stack buffer.
///
/// Every `console::write_*` call takes the writer lock separately, so a line
/// built from several calls can be cut in half by another CPU (the task-exit
/// trace is the visible case: a user task printing on CPU1 lands inside it).
/// Building the line first and emitting it with one [`Line::emit`] call makes
/// the whole line atomic without widening the writer lock around kernel code.
pub struct Line {
    buffer: [u8; Line::CAPACITY],
    len: usize,
}

impl Line {
    pub const CAPACITY: usize = 128;

    pub const fn new() -> Self {
        Self {
            buffer: [0; Self::CAPACITY],
            len: 0,
        }
    }

    pub fn str(&mut self, text: &str) -> &mut Self {
        let room = Self::CAPACITY - self.len;
        let take = text.len().min(room);
        self.buffer[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        self
    }

    pub fn byte(&mut self, byte: u8) -> &mut Self {
        if self.len < Self::CAPACITY {
            self.buffer[self.len] = byte;
            self.len += 1;
        }
        self
    }

    /// Appends `value` in decimal. A value that does not fit is dropped rather
    /// than printed truncated: a wrong number is worse than a short line.
    pub fn dec(&mut self, value: u64) -> &mut Self {
        let mut digits = [0_u8; 20];
        let mut count = 0;
        let mut remaining = value;
        loop {
            digits[count] = b'0' + (remaining % 10) as u8;
            count += 1;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        if Self::CAPACITY - self.len >= count {
            for index in 0..count {
                self.buffer[self.len + index] = digits[count - 1 - index];
            }
            self.len += count;
        }
        self
    }

    /// Appends `value` as 16 zero-padded hex digits.
    pub fn hex_u64(&mut self, value: u64) -> &mut Self {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        if Self::CAPACITY - self.len >= 16 {
            for index in 0..16 {
                self.buffer[self.len + index] = HEX[((value >> (60 - index * 4)) & 0xF) as usize];
            }
            self.len += 16;
        }
        self
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        let room = Self::CAPACITY - self.len;
        let take = bytes.len().min(room);
        self.buffer[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        self
    }

    pub fn data(&self) -> &[u8] {
        &self.buffer[..self.len]
    }

    /// Writes the assembled line in one call (one writer-lock acquisition).
    pub fn emit(&self) -> usize {
        if self.len == 0 {
            return 0;
        }
        write_bytes(self.data());
        self.len
    }
}

pub fn write_byte(byte: u8) {
    let _writer = WriterGuard::acquire();
    match active() {
        Active::Geni => crate::geni_uart::write_byte(byte),
        Active::Pl011 => crate::pl011::write_byte(byte),
        Active::None => {}
    }
}

pub fn write_str(message: &str) {
    let _writer = WriterGuard::acquire();
    match active() {
        Active::Geni => crate::geni_uart::write_str(message),
        Active::Pl011 => crate::pl011::write_str(message),
        Active::None => {}
    }
}

pub fn write_bytes(bytes: &[u8]) {
    let _writer = WriterGuard::acquire();
    match active() {
        Active::Geni => crate::geni_uart::write_bytes(bytes),
        Active::Pl011 => crate::pl011::write_bytes(bytes),
        Active::None => {}
    }
}

pub fn write_hex_u64(value: u64) {
    let _writer = WriterGuard::acquire();
    match active() {
        Active::Geni => crate::geni_uart::write_hex_u64(value),
        Active::Pl011 => crate::pl011::write_hex_u64(value),
        Active::None => {}
    }
}

pub fn write_dec_u64(value: u64) {
    let _writer = WriterGuard::acquire();
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
