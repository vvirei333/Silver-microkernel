//! PSCI power management.
//!
//! The conduit is a property of the firmware, not a build guess. The live
//! `/psci` node names it (`method = "hvc"` or `method = "smc"`); QEMU `virt`
//! uses HVC, while Qualcomm and Raspberry Pi firmware use the secure monitor.
//! Calling the wrong one traps as an unknown instruction, which is exactly
//! what happened to the first fog image.
//!
//! The tree value is cached once, before the console starts. When the tree has
//! no `/psci` node the platform identity is the fallback, preserving the QEMU
//! and Qualcomm behaviour this module had before.
//!
//! `SYSTEM_RESET` is also used by the diagnostic `fog-auto-reset` build, which
//! boots from RAM and returns the unit to Android without a button hold. No
//! path here touches flash.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::fdt::{Fdt, PsciMethod};

/// PSCI `SYSTEM_OFF` (0.2+), no arguments.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
/// PSCI `SYSTEM_RESET` (0.2+), no arguments.
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
/// PSCI `CPU_ON` (64-bit), arguments: target MPIDR, entry, context id.
pub(crate) const PSCI_CPU_ON: u64 = 0xC400_0003;

const CONDUIT_UNKNOWN: u8 = 0;
const CONDUIT_HVC: u8 = 1;
const CONDUIT_SMC: u8 = 2;

static CONDUIT: AtomicU8 = AtomicU8::new(CONDUIT_UNKNOWN);

/// Records the conduit the live tree declares, before any PSCI call.
pub fn init(dtb: &[u8]) {
    let Some(tree) = Fdt::new(dtb) else {
        return;
    };
    let conduit = match tree.psci_method() {
        Some(PsciMethod::Hvc) => CONDUIT_HVC,
        Some(PsciMethod::Smc) => CONDUIT_SMC,
        None => return,
    };
    CONDUIT.store(conduit, Ordering::Release);
}

fn conduit() -> u8 {
    let cached = CONDUIT.load(Ordering::Acquire);
    if cached != CONDUIT_UNKNOWN {
        return cached;
    }

    // No `/psci` method was available. This is the pre-existing platform
    // fallback: QEMU virt exposes PSCI over HVC, everything else uses SMC.
    let fallback = match crate::platform::detect() {
        crate::platform::Kind::Qemu => CONDUIT_HVC,
        _ => CONDUIT_SMC,
    };
    match CONDUIT.compare_exchange(
        CONDUIT_UNKNOWN,
        fallback,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => fallback,
        Err(other) => other,
    }
}

/// Issues one PSCI call through the conduit the firmware declared.
pub(crate) fn call(function: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let result: u64;
    unsafe {
        if conduit() == CONDUIT_HVC {
            asm!(
                "hvc #0",
                inlateout("x0") function => result,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                options(nomem, nostack),
            );
        } else {
            asm!(
                "smc #0",
                inlateout("x0") function => result,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                options(nomem, nostack),
            );
        }
    }
    result
}

/// Powers the machine down through PSCI.
///
/// Never returns on success. If no PSCI conduit exists the call traps; the
/// exception vector turns that into a visible kernel panic instead of
/// continuing with an unknown power state.
pub fn system_off() -> ! {
    let _ = call(PSCI_SYSTEM_OFF, 0, 0, 0);
    crate::exceptions::silver_handle_exception(4, 0, 0, 0)
}

/// Resets the machine through PSCI.
pub fn system_reset() -> ! {
    let _ = call(PSCI_SYSTEM_RESET, 0, 0, 0);
    crate::exceptions::silver_handle_exception(4, 0, 0, 0)
}