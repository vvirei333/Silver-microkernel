use core::sync::atomic::{AtomicU64, Ordering};

pub const TICK_INTERVAL_MS: u64 = 10;

static INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

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
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFF00_FFFF); // cyan: timer armed
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
    // global time advances on the boot CPU alone, otherwise N CPUs would
    // count N ticks per interval and every deadline would compress N-fold.
    if crate::smp::this_cpu_id() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    crate::smp::probe(21);
    crate::smp::TICK_PROBE.bump();
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
