//! Boot timing using the ARM64 architectural counter.
//!
//! `cntpct_el0` is a free-running 64-bit counter that counts up from reset at
//! the rate reported by `cntfrq_el0`. We snapshot the counter at the very start
//! of `kernel_main` and report each milestone as elapsed seconds with
//! millisecond precision: `[ 0.123 s ] MMU init`.

use core::sync::atomic::{AtomicU64, Ordering};

/// Fallback counter frequency when `cntfrq_el0` reads zero (some firmware
/// leaves it unset). 0x124F800 == 19.2 MHz, the SM8650 arch-timer rate from
/// the tablet's DTB (`arch_timer { clock-frequency = <0x124f800> }`).
const FALLBACK_FREQUENCY_HZ: u64 = 0x124F800;

static FREQUENCY_HZ: AtomicU64 = AtomicU64::new(FALLBACK_FREQUENCY_HZ);
static START_COUNT: AtomicU64 = AtomicU64::new(0);

/// Snapshots the counter frequency and the t=0 reference. Must be called once,
/// as early as possible in `kernel_main`, before any milestone is logged.
pub fn mark_boot_start() {
    let mut frequency = counter_frequency();
    if frequency == 0 {
        frequency = FALLBACK_FREQUENCY_HZ;
    }
    FREQUENCY_HZ.store(frequency, Ordering::Relaxed);
    START_COUNT.store(counter_value(), Ordering::Relaxed);
}

/// Prints `[ s.mmm s ] name\r\n` where `s.mmm` is the time since
/// `mark_boot_start`, with the milliseconds zero-padded to three digits.
pub fn log_milestone(name: &str) {
    let start = START_COUNT.load(Ordering::Relaxed);
    let frequency = FREQUENCY_HZ.load(Ordering::Relaxed).max(1);
    let elapsed = counter_value().wrapping_sub(start);

    // Seconds and milliseconds, avoiding floating point entirely.
    let total_ms = elapsed / frequency * 1000 + (elapsed % frequency) * 1000 / frequency;
    let seconds = total_ms / 1000;
    let millis = total_ms % 1000;

    crate::console::write_str("[ ");
    crate::console::write_dec_u64(seconds);
    crate::console::write_byte(b'.');
    // Zero-pad the millisecond field to three digits.
    crate::console::write_byte(b'0' + (millis / 100) as u8);
    crate::console::write_byte(b'0' + ((millis / 10) % 10) as u8);
    crate::console::write_byte(b'0' + (millis % 10) as u8);
    crate::console::write_str(" s ] ");
    crate::console::write_str(name);
    crate::console::write_str("\r\n");
}

fn counter_frequency() -> u64 {
    let frequency: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack));
    }
    frequency
}

fn counter_value() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) value, options(nomem, nostack));
    }
    value
}
