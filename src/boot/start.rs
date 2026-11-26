//! Final boot milestone: hand the machine to the scheduler.
//!
//! The fog hardware profile starts the scheduler straight away; every other
//! profile prints the boot-complete milestone first. Either way `run` does
//! not return.

#[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
use crate::boot_metrics;
#[cfg(any(not(feature = "tablet"), feature = "fog"))]
use crate::console;
#[cfg(not(feature = "fog"))]
use crate::ram_log;
use crate::scheduler;

/// Initialises the scheduler and starts it. Never returns.
pub fn run() -> ! {
    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER PLATFORM READY\r\n");
        scheduler::init_fog();
        console::write_str("SILVER SCHEDULER START\r\n");
        scheduler::start();
    }
    #[cfg(not(feature = "fog"))]
    {
        scheduler::init();

        #[cfg(not(feature = "tablet"))]
        {
            console::write_str("IRQ READY\r\n");
            for &byte in b"HELLO, WORLD!\r\n" {
                console::write_byte(byte);
            }
            boot_metrics::log_milestone("Scheduler start");
            console::write_str("SCHEDULER START\r\n");
        }
        ram_log::log("SCHEDULER START - BOOT COMPLETE");
        crate::log_info_why!(
            "boot",
            "all init milestones reached",
            "boot complete, handing control to the scheduler"
        );
        scheduler::start();
    }
}
