//! Runtime platform identity: QEMU `virt`, the SM8650 tablet, the SM6225
//! Redmi 10C, or an explicitly unknown machine.
//!
//! The boot path needs one source of truth for "where am I?" because UART,
//! GIC, DRAM and the kernel load address differ per target. Two independent
//! signals are used:
//!
//! * the live FDT root `compatible` (authoritative when a DTB was passed),
//! * `MIDR_EL1` as a fallback for the two emulated/reference targets.
//!
//! An unrecognised CPU is **not** silently treated as the tablet any more:
//! that used to route MMIO to SM8650 addresses on any unknown AArch64 part,
//! which is a bus abort waiting to happen. `Kind::Unknown` makes the console
//! and interrupt paths fail closed instead.

use core::sync::atomic::{AtomicU8, Ordering};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    /// QEMU `virt` (`-cpu cortex-a72`): PL011 UART, GICv2, DRAM @ 0x4000_0000.
    Qemu,
    /// Legion Y700 (SM8650): GENI UART, GICv3, kernel @ 0xA800_0000.
    Sm8650,
    /// Xiaomi Redmi 10C (SM6225 "fog"/"khaje"): GENI UART, GICv3.
    Fog,
    /// No reliable identification. Every device path must fail closed.
    Unknown,
}

impl Kind {
    pub fn is_tablet(self) -> bool {
        matches!(self, Kind::Sm8650)
    }

    pub fn is_fog(self) -> bool {
        matches!(self, Kind::Fog)
    }

    /// Both SM8650 and SM6225 use a GICv3 with a system-register CPU
    /// interface; QEMU `virt` and `Unknown` do not.
    pub fn uses_gicv3(self) -> bool {
        matches!(self, Kind::Sm8650 | Kind::Fog)
    }
}

const UNINITIALISED: u8 = 0;
const TAG_QEMU: u8 = 1;
const TAG_SM8650: u8 = 2;
const TAG_FOG: u8 = 3;
const TAG_UNKNOWN: u8 = 4;

static KIND: AtomicU8 = AtomicU8::new(UNINITIALISED);

fn encode(kind: Kind) -> u8 {
    match kind {
        Kind::Qemu => TAG_QEMU,
        Kind::Sm8650 => TAG_SM8650,
        Kind::Fog => TAG_FOG,
        Kind::Unknown => TAG_UNKNOWN,
    }
}

fn decode(tag: u8) -> Option<Kind> {
    match tag {
        TAG_QEMU => Some(Kind::Qemu),
        TAG_SM8650 => Some(Kind::Sm8650),
        TAG_FOG => Some(Kind::Fog),
        TAG_UNKNOWN => Some(Kind::Unknown),
        _ => None,
    }
}

/// Records the identity proved by the live FDT. Called once, before the
/// console is initialised.
pub fn force(kind: Kind) {
    KIND.store(encode(kind), Ordering::Release);
}

/// Returns the cached identity, detecting it from `MIDR_EL1` on first use.
pub fn detect() -> Kind {
    let tag = KIND.load(Ordering::Acquire);
    if let Some(kind) = decode(tag) {
        return kind;
    }

    let part = (midr_el1() >> 4) & 0xFFF;
    let kind = match part {
        // ARM Cortex-A72 — only ever our QEMU `-cpu cortex-a72`.
        0xD08 => Kind::Qemu,
        // SM8650 Kryo cluster (Cortex-X4 / A720 / A520).
        0xD80..=0xD82 => Kind::Sm8650,
        // Everything else must be identified from the FDT; guessing here is
        // what this enum exists to prevent.
        _ => Kind::Unknown,
    };
    KIND.store(encode(kind), Ordering::Release);
    kind
}

fn midr_el1() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, midr_el1", out(reg) value, options(nomem, nostack));
    }
    value
}
