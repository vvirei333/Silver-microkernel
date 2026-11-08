use core::sync::atomic::{AtomicU64, Ordering};

pub const TICK_INTERVAL_MS: u64 = 10;

static INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
/// System-counter value captured by [`init`]. Global tick time is derived from
/// `CNTPCT_EL0` relative to this base, so it is one monotonic clock for every
/// PE and keeps advancing while a CPU cannot take its own timer PPI.
static COUNTER_BASE: AtomicU64 = AtomicU64::new(0);

/// Per-CPU tick counters. They are liveness evidence only: what identifies a
/// core that is online yet no longer taking its timer interrupt. Global time
/// comes from the system counter, not from this array, so N CPUs cannot make
/// time run N times faster.
static CPU_TICKS: [AtomicU64; crate::smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];

/// Ticks observed on `cpu` (0 when the index is out of range). The stall
/// watchdog and the `smp-selftest` liveness check consume this.
pub fn cpu_ticks(cpu: usize) -> u64 {
    CPU_TICKS
        .get(cpu)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn init() {
    let mut frequency = counter_frequency();
    if frequency == 0 {
        // CNTFRQ_EL0 is architecturally required to be programmed by the
        // firmware, but a hand-over bug must not turn every deadline into a
        // zero-length interval. 19.2 MHz is the khaje/khaje-family crystal
        // value documented in the platform DTS; log loudly when it is used.
        frequency = 19_200_000;
        crate::console::write_str("TIMER: CNTFRQ_EL0=0, assuming 19.2 MHz\r\n");
    }
    let interval = frequency * TICK_INTERVAL_MS / 1000;
    let interval = if interval == 0 { 1 } else { interval };

    // Capture the base before publishing the interval: `ticks()` falls back to
    // the interrupt counter while the interval is still zero, and once it sees
    // a non-zero interval the base is already valid.
    COUNTER_BASE.store(read_counter(), Ordering::Relaxed);
    INTERVAL_TICKS.store(interval, Ordering::Relaxed);

    if !available() {
        // No confirmed redistributor: arming CNTP can never produce a tick,
        // and the PPI is deliberately not enabled. Leave the counter off so
        // the scheduler does not look healthy while preemption is impossible.
        #[cfg(feature = "tablet")]
        crate::ram_log::log("TIMER: DISABLED (no GIC redistributor)");
        #[cfg(not(feature = "tablet"))]
        crate::console::write_str("TIMER: DISABLED (no confirmed GIC redistributor)\r\n");
        return;
    }
    rearm();
    set_control(1);
}

/// Whether timer interrupts can actually be delivered on this platform.
pub fn available() -> bool {
    crate::gic::timer_available()
}

/// Per-CPU timer bring-up for APs: the interval is global (set by the boot
/// CPU in `init`); each AP arms its own physical timer and enables it so its
/// preemption tick runs. An AP without a confirmed redistributor stays off.
pub fn cpu_init() {
    if !available() {
        return;
    }
    rearm();
    set_control(1);
}

pub fn handle_irq() {
    // Every CPU has its own timer PPI, but they exist for preemption only:
    // global time comes from `CNTPCT_EL0` (see `ticks`), otherwise N CPUs
    // would count N ticks per interval and every deadline would compress
    // N-fold. The per-CPU interrupt counter still advances so a live core can
    // be distinguished from one that is online but no longer receiving its
    // PPI, and `TICKS` stays as the pre-init fallback.
    let cpu = crate::smp::this_cpu_id();
    if let Some(counter) = CPU_TICKS.get(cpu) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    if cpu == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    crate::smp::probe(21);
    crate::smp::TICK_PROBE.bump();
    #[cfg(feature = "smp")]
    crate::watchdog::on_timer_tick(cpu);
    rearm();
}

/// Monotonic global time in [`TICK_INTERVAL_MS`] units.
///
/// The value comes from the always-on system counter, not from the boot CPU's
/// interrupt handler: deadlines must not stop while CPU0 is in a long
/// IRQ-masked section (a kernel lock held across a stop-the-world round, a
/// diagnostic spin, or the churn harness's wait). `TICKS` remains the
/// interrupt-driven fallback for the window before `init` publishes an
/// interval.
pub fn ticks() -> u64 {
    let interval = INTERVAL_TICKS.load(Ordering::Relaxed);
    if interval == 0 {
        return TICKS.load(Ordering::Relaxed);
    }
    read_counter().saturating_sub(COUNTER_BASE.load(Ordering::Relaxed)) / interval
}

/// Reads the EL1 physical counter (`CNTPCT_EL0`), the same time base the
/// physical timer compares against.
fn read_counter() -> u64 {
    let counter: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) counter, options(nomem, nostack));
    }
    counter
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
