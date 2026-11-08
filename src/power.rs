//! PSCI power management.
//!
//! The conduit is part of the platform contract, not a build guess:
//! QEMU `virt` exposes PSCI over `hvc` (requires `-machine virt,virtualization=on`),
//! while Qualcomm platforms (fog/khaje and the tablet) use the secure monitor
//! (`smc`). Calling the wrong one traps as an unknown instruction, which is
//! exactly what happened to the first fog image.
//!
//! `SYSTEM_RESET` is also used by the diagnostic `fog-auto-reset` build, which
//! boots from RAM and returns the unit to Android without a button hold. No
//! path here touches flash.

use core::arch::asm;

/// PSCI `SYSTEM_OFF` (0.2+), no arguments.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
/// PSCI `SYSTEM_RESET` (0.2+), no arguments.
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

fn psci_call(function: u64) {
    match crate::platform::detect() {
        crate::platform::Kind::Qemu => unsafe {
            // QEMU `virt` exposes PSCI over HVC once EL2 is present
            // (`-machine virt,virtualization=on`).
            asm!(
                "hvc #0",
                in("x0") function,
                in("x1") 0_u64,
                in("x2") 0_u64,
                in("x3") 0_u64,
                options(nomem, nostack),
            );
        },
        _ => unsafe {
            asm!(
                "smc #0",
                in("x0") function,
                in("x1") 0_u64,
                in("x2") 0_u64,
                in("x3") 0_u64,
                options(nomem, nostack),
            );
        },
    }
}

/// Powers the machine down through PSCI.
///
/// Never returns on success. If no PSCI conduit exists the call traps; the
/// exception vector turns that into a visible kernel panic instead of
/// continuing with an unknown power state.
pub fn system_off() -> ! {
    psci_call(PSCI_SYSTEM_OFF);
    crate::exceptions::silver_handle_exception(4, 0, 0, 0)
}

/// Resets the machine through PSCI.
pub fn system_reset() -> ! {
    psci_call(PSCI_SYSTEM_RESET);
    crate::exceptions::silver_handle_exception(4, 0, 0, 0)
}
