//! PSCI reset for the real-hardware profiles.
//!
//! QEMU `virt` exposes PSCI through `hvc`. Qualcomm platforms (fog/khaje and
//! the tablet) use the secure-monitor conduit instead, so `smc #0` is the only
//! call that reaches the firmware. The conduit is selected from the platform
//! identity proved at boot, never from a build-time guess.
//!
//! `SYSTEM_RESET` is used only by diagnostic builds (`fog-auto-reset`), which
//! boot from RAM and must return the unit to Android without a button hold.
//! It never touches flash.

use core::arch::asm;

/// PSCI `SYSTEM_RESET` (0.2+), no arguments.
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// Resets the machine through the platform's PSCI conduit.
///
/// Never returns. If the firmware refuses (or no PSCI exists), the call traps
/// or falls through; the latter is turned into a visible panic instead of
/// silently continuing with unknown power state.
pub fn system_reset() -> ! {
    // QEMU `virt` without EL2 exposes PSCI through the emulated secure
    // monitor (`smc`); Qualcomm uses the same conduit. `hvc` is only valid
    // when EL2 exists and would fault as an unknown instruction otherwise, so
    // one SMC path covers both targets.
    unsafe {
        asm!(
            "smc #0",
            in("x0") PSCI_SYSTEM_RESET,
            in("x1") 0_u64,
            in("x2") 0_u64,
            in("x3") 0_u64,
            options(nomem, nostack),
        );
    }
    crate::exceptions::silver_handle_exception(4, 0, 0, 0)
}
