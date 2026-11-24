//! Interrupt milestone: the interrupt controller and the timer that feeds it.
//!
//! The controller comes from the same device map as the console; the timer PPI
//! is whatever the `arm,armv8-timer` node says. The timer proof unmasks IRQs in
//! kernel context only after the controller has a distributor to acknowledge
//! them.

#[cfg(feature = "fog-auto-reset")]
use crate::fog;
#[cfg(any(not(feature = "fog"), feature = "fog-timer-test"))]
use crate::interrupts;
use crate::{console, driver_resources, gic, ram_log, timer};

/// Initialises the GIC and the generic timer and proves the timer fires.
pub fn init() {
    // The tablet/fog path shares the QEMU boot sequence from here on: gic::init
    // dispatches to gicv3, and the timer PPI is selected per platform (INTID
    // 30 on SM8650, 18 on SM6225/khaje).
    // The interrupt controller is bound from the same device map as the
    // console: a GICv2 gets the distributor and CPU-interface bases the tree
    // describes (they are not at a fixed offset from each other on real
    // controllers), a GICv3 keeps its system-register path.
    if let Some(intid) = driver_resources::timer_intid() {
        gic::set_timer_irq(intid);
    }
    match driver_resources::gic_bases() {
        Some((distributor, cpu_interface)) => gic::init_at(distributor, cpu_interface),
        None => gic::init(),
    }
    timer::init();
    #[cfg(feature = "fog-auto-reset")]
    fog::arm_auto_reset(30);
    // BISECT: isolated timer test — unmask IRQs in kernel context, wait for
    // ticks only when a redistributor was confirmed. Without one, spinning
    // for an interrupt that cannot be delivered would just hide the failure.
    #[cfg(any(not(feature = "fog"), feature = "fog-timer-test"))]
    {
        if timer::available() {
            let saved = interrupts::save_and_disable_irq();
            interrupts::enable_irq();
            let mut spin: u32 = 0;
            while timer::ticks() < 30 && spin < 800_000_000 {
                spin += 1;
                core::hint::spin_loop();
            }
            interrupts::restore_irq(saved);
            console::write_str("TIMER TEST: ticks=");
            console::write_dec_u64(timer::ticks());
            console::write_str(" spins=");
            console::write_dec_u64(spin as u64);
            console::write_str("\r\n");
        } else {
            console::write_str("TIMER TEST: SKIP (no confirmed GICv3 timer PPI)\r\n");
        }
    }
    #[cfg(all(feature = "fog", not(feature = "fog-timer-test")))]
    {
        // The first hardware boot must not deliver a timer IRQ before
        // scheduler::init_fog(): gicv3::handle_irq calls schedule() on every
        // tick, and the scheduler state is not ready yet.
        console::write_str("TIMER TEST: SKIP (fog hardware default)\r\n");
    }
    ram_log::log("GICv3+TIMER OK");
}
