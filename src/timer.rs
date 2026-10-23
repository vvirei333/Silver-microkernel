use core::sync::atomic::{AtomicU64, Ordering};

pub const TICK_INTERVAL_MS: u64 = 10;

static INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let frequency = counter_frequency();
    let interval = frequency * TICK_INTERVAL_MS / 1000;
    let interval = if interval == 0 { 1 } else { interval };

    INTERVAL_TICKS.store(interval, Ordering::Relaxed);
    rearm();
    set_control(1);
}

pub fn handle_irq() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    rearm();
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn milliseconds_to_ticks(milliseconds: u64) -> u64 {
    milliseconds.div_ceil(TICK_INTERVAL_MS)
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
