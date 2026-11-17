//! ARMv8 hardware watchpoints for the GDB stub.
//!
//! One write watchpoint (DBGWCR0/DBGWVR0) plus the debug-exception enable in
//! MDSCR_EL1. The kernel has no per-task watcher bookkeeping: a watchpoint is
//! a debugging aid armed by the native shell, and the first write anywhere on
//! the machine reports the writer. That is exactly what the phoc corruption
//! needs: arm the field, boot the guest, and catch the store.

use core::sync::atomic::{AtomicU64, Ordering};

static ARMED: AtomicU64 = AtomicU64::new(0);

const EC_WATCHPOINT_LOW: u64 = 0x34;

fn write_wcr(value: u64) {
    unsafe {
        core::arch::asm!("msr dbgwcr0_el1, {}", in(reg) value, options(nostack));
    }
}

fn write_wvr(value: u64) {
    unsafe {
        core::arch::asm!("msr dbgwvr0_el1, {}", in(reg) value, options(nostack));
    }
    unsafe {
        core::arch::asm!("isb", options(nostack));
    }
}

fn enable_monitors(enable: bool) {
    unsafe {
        let mut mdscr: u64;
        core::arch::asm!("mrs {}, mdscr_el1", out(reg) mdscr, options(nostack));
        if enable {
            // MDE (15): breakpoint/watchpoint exceptions, KDE (13): EL1 debug.
            mdscr |= (1 << 15) | (1 << 13);
        } else {
            mdscr &= !(1 << 15);
        }
        core::arch::asm!("msr mdscr_el1, {}", in(reg) mdscr, options(nostack));
        core::arch::asm!("isb", options(nostack));
    }
}

/// Watches `length` bytes at `address` for writes. Length 1..=8 and the range
/// is rounded to the containing 8-byte granule, which is what DBGWVR/BAS give.
pub fn arm_write(address: u64, length: u64) -> bool {
    if length == 0 || length > 8 || address < 0x1000 || address >= 0x0000_8000_0000_0000 {
        return false;
    }
    let start = (address & !7) as usize;
    let first = (address & 7) as u32;
    let last = (first as u64 + length - 1).min(7) as u32;
    let mut bas = 0u64;
    for bit in first..=last {
        bas |= 1 << bit;
    }
    // E=1, PAC=0, LSC=0b10 (store), BAS, everything else 0.
    let wcr = 1 | (0b10 << 3) | (bas << 5);
    write_wvr(start as u64);
    write_wcr(wcr);
    enable_monitors(true);
    let (read_wcr, read_wvr): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, dbgwcr0_el1", out(reg) read_wcr, options(nostack));
        core::arch::asm!("mrs {}, dbgwvr0_el1", out(reg) read_wvr, options(nostack));
    }
    if read_wcr != wcr || read_wvr != start as u64 {
        disarm();
        return false;
    }
    ARMED.store(address, Ordering::Release);
    true
}

pub fn disarm() {
    write_wcr(0);
    write_wvr(0);
    enable_monitors(false);
    ARMED.store(0, Ordering::Release);
}

pub fn armed() -> u64 {
    ARMED.load(Ordering::Acquire)
}

pub fn is_watchpoint(esr: u64) -> bool {
    ((esr >> 26) & 0x3f) == EC_WATCHPOINT_LOW
}
