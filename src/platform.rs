//! Runtime platform detection: is the kernel running under QEMU `virt` or on
//! the real Snapdragon 8 Gen 3 (SM8650 "pineapple") hardware?
//!
//! The two targets differ in almost every device (UART, GIC, DRAM, PSCI), so
//! the boot path needs a single source of truth for "where am I?". We identify
//! the platform from `MIDR_EL1`: QEMU is always launched with `-cpu cortex-a72`
//! (part number `0xD08`), while the tablet boots on a Kryo cluster (Cortex-X4 /
//! A720 / A520, part numbers `0xD80..0xD82`).
//!
//! This is deliberately a small, self-contained discriminator. A full FDT walk
//! (the pointer the bootloader passes in `x0`) is the more general solution and
//! can replace this later without changing the rest of the kernel.

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    /// QEMU `virt` (`-cpu cortex-a72`): PL011 UART, GICv2, DRAM @ 0x4000_0000.
    Qemu,
    /// Legion Y700 (SM8650): GENI UART, GICv3, kernel @ 0xA800_0000.
    Sm8650,
}

impl Kind {
    pub fn is_tablet(self) -> bool {
        matches!(self, Kind::Sm8650)
    }
}

/// Reads `MIDR_EL1` once and classifies the platform. The result is stable for
/// the whole boot, so callers may cache it; this function itself is cheap and
/// can be called as needed.
pub fn detect() -> Kind {
    let part = (midr_el1() >> 4) & 0xFFF;
    if part == 0xD08 {
        // ARM Cortex-A72 — only ever our QEMU `-cpu cortex-a72`.
        Kind::Qemu
    } else {
        // Everything else in our fleet is the SM8650 Kryo cluster.
        Kind::Sm8650
    }
}

fn midr_el1() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, midr_el1", out(reg) value, options(nomem, nostack));
    }
    value
}
