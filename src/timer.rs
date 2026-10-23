use core::sync::atomic::{AtomicU64, Ordering};

use crate::pl011;

static INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let frequency = counter_frequency();
    let interval = frequency / 10;
    let interval = if interval == 0 { 1 } else { interval };

    INTERVAL_TICKS.store(interval, Ordering::Relaxed);
    rearm();
    set_control(1);
}

pub fn handle_irq() {
    rearm();
    let ticks = TICKS.fetch_add(1, Ordering::Relaxed) + 1;

    if ticks == 1 {
        pl011::write_str("TIMER IRQ OK\r\n");
    }
}

fn counter_frequency() -> u64 {
    let frequency: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack));
    }
    frequency
}

fn rearm() {
    let interval = INTERVAL_TICKS.load(Ordering::Relaxed);
    unsafe {
        core::arch::asm!(
            "msr cntp_tval_el0, {interval}",
            interval = in(reg) interval,
            options(nomem, nostack),
        );
    }
}

fn set_control(control: u64) {
    unsafe {
        core::arch::asm!(
            "msr cntp_ctl_el0, {control}",
            control = in(reg) control,
            options(nomem, nostack),
        );
    }
}
