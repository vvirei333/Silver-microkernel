//! One panic banner and a stable numeric code for every fatal kernel path.
//!
//! A fault used to print a register dump whose heading was merely
//! "KERNEL PANIC: halting"; there was no single value to quote in a bug
//! report. Every fatal path now funnels through [`halt`], which prints
//! `KERNEL PANIC code=0x.. (name)` first, then the exception registers when
//! the caller supplied them, writes the same code to ramoops and parks.

/// A Rust `panic!` (or an explicit assertion in kernel code).
pub const RUST_PANIC: u64 = 0x01;
/// A synchronous exception taken from EL1.
pub const SYNC_EXCEPTION: u64 = 0x02;
/// A FIQ with no owner.
pub const FIQ: u64 = 0x03;
/// An SError interrupt.
pub const SERROR: u64 = 0x04;
/// A boot-time invariant failed before userspace existed.
pub const BOOT: u64 = 0x05;
/// The frame or heap allocator found its own state inconsistent.
pub const ALLOCATOR: u64 = 0x06;
/// An IRQ arrived with no registered handler.
pub const IRQ: u64 = 0x07;
/// A boot self-test failed.
pub const SELFTEST: u64 = 0x08;
/// PSCI power-off/reset failed.
pub const POWER: u64 = 0x09;
/// An internal kind no public code covers yet.
pub const UNKNOWN: u64 = 0xff;

/// Maps the exception entry's internal `kind` to (code, printable name).
///
/// The boot vectors pass 0 for an EL1 sync exception, 2 for FIQ, 3 for
/// SError; Rust code passes 4 (panic), 6 (boot assertion), 7 (allocator),
/// 8 (unexpected IRQ), 9 (self-test) and 10 (PSCI).
pub fn code_for_kind(kind: u64) -> (u64, &'static str) {
    match kind {
        0 => (SYNC_EXCEPTION, "sync exception"),
        2 => (FIQ, "fiq"),
        3 => (SERROR, "serror"),
        4 => (RUST_PANIC, "rust panic"),
        6 => (BOOT, "boot assertion"),
        7 => (ALLOCATOR, "allocator"),
        8 => (IRQ, "unexpected irq"),
        9 => (SELFTEST, "self-test"),
        10 => (POWER, "power off"),
        _ => (UNKNOWN, "unknown"),
    }
}

/// Prints the first line of a panic; no-op on the tablet, whose console must
/// not be touched while the MMU state is unknown.
pub fn banner(code: u64, name: &str) {
    #[cfg(not(feature = "tablet"))]
    {
        // History first: the last few hundred bytes explain how the machine
        // reached this point, and the banner below is not repeated in it.
        crate::log::dump_tail(1536);
        crate::console::write_str("KERNEL PANIC code=");
        write_code(code);
        crate::console::write_str(" (");
        crate::console::write_str(name);
        crate::console::write_str(")\r\n");
    }
    #[cfg(feature = "tablet")]
    {
        let _ = (code, name);
    }
}

/// Prints a panic code as `0xNN` (the defined codes fit in one byte) and
/// falls back to the full width for an out-of-range internal value.
#[cfg(not(feature = "tablet"))]
fn write_code(code: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if code > 0xff {
        crate::console::write_str("0x");
        crate::console::write_hex_u64(code);
        return;
    }
    let text = [
        b'0',
        b'x',
        HEX[((code >> 4) & 0xf) as usize],
        HEX[(code & 0xf) as usize],
    ];
    if let Ok(text) = core::str::from_utf8(&text) {
        crate::console::write_str(text);
    }
}

/// Prints the fault registers only when the caller has real values to show.
pub fn details(esr: u64, elr: u64, far: u64) {
    #[cfg(not(feature = "tablet"))]
    {
        if esr == 0 && elr == 0 && far == 0 {
            return;
        }
        crate::console::write_str("ESR_EL1=");
        crate::console::write_hex_u64(esr);
        crate::console::write_str(" ELR_EL1=");
        crate::console::write_hex_u64(elr);
        crate::console::write_str(" FAR_EL1=");
        crate::console::write_hex_u64(far);
        crate::console::write_str("\r\n");
    }
    #[cfg(feature = "tablet")]
    {
        let _ = (esr, elr, far);
    }
}

/// Logs the code and registers to ramoops and halts forever.
pub fn park(code: u64, esr: u64, elr: u64, far: u64) -> ! {
    crate::ram_log::log_raw("PANIC code=");
    crate::ram_log::log_hex(code);
    if esr != 0 || elr != 0 || far != 0 {
        crate::ram_log::log_raw(" ESR=");
        crate::ram_log::log_hex(esr);
        crate::ram_log::log_raw(" ELR=");
        crate::ram_log::log_hex(elr);
        crate::ram_log::log_raw(" FAR=");
        crate::ram_log::log_hex(far);
    }
    crate::ram_log::log_raw("\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}

/// Banner, optional registers, log, halt.
pub fn halt(code: u64, name: &str, esr: u64, elr: u64, far: u64) -> ! {
    banner(code, name);
    details(esr, elr, far);
    park(code, esr, elr, far)
}

