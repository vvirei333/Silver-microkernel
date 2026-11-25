//! Platform identity: which machine are we on, and how do we prove it?
//!
//! This module is the *pure* half of platform detection: the identity enum,
//! the device-tree `compatible` matcher and the cached answer. It lives in the
//! shared library so both the kernel and `cargo test --lib` can exercise it;
//! the hardware half (`MIDR_EL1` reads, called from `src/platform.rs`) stays in
//! the kernel binary because reading a system register cannot be tested on an
//! x86_64 host.
//!
//! Why this exists at all: UART, GIC, DRAM and the kernel load address differ
//! per machine. An unrecognised CPU must end up as [`Kind::Unknown`] so every
//! device path fails closed — routing MMIO to another SoC's addresses is a bus
//! abort, not a graceful degradation.

use core::sync::atomic::{AtomicU8, Ordering};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    /// QEMU `virt` (`-cpu cortex-a72`): PL011 UART, GICv2, DRAM @ 0x4000_0000.
    Qemu,
    /// Legion Y700 (SM8650): GENI UART, GICv3, kernel @ 0xA800_0000.
    Sm8650,
    /// Xiaomi Redmi 10C (SM6225 "fog"/"khaje"): GENI UART, GICv3.
    Fog,
    /// Raspberry Pi 5 (BCM2712, Cortex-A76): PL011 debug UART, GIC-400.
    ///
    /// Only the identity is wired here (from the live device tree). The console
    /// and interrupt paths for this board do not exist yet; the generic SDHCI
    /// storage core does, but has no BCM2712 hardware log — see `docs/PI5.md`.
    /// A BCM2712 boot must fail closed rather than fall into another
    /// platform's MMIO map.
    Bcm2712,
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
    /// interface; QEMU `virt`, BCM2712 (GIC-400 = GICv2) and `Unknown` do not.
    pub fn uses_gicv3(self) -> bool {
        matches!(self, Kind::Sm8650 | Kind::Fog)
    }

    /// Whether a physical board (as opposed to an emulator) is claimed.
    pub fn is_board(self) -> bool {
        matches!(self, Kind::Sm8650 | Kind::Fog | Kind::Bcm2712)
    }
}

const UNINITIALISED: u8 = 0;
const TAG_QEMU: u8 = 1;
const TAG_SM8650: u8 = 2;
const TAG_FOG: u8 = 3;
const TAG_UNKNOWN: u8 = 4;
const TAG_BCM2712: u8 = 5;

static KIND: AtomicU8 = AtomicU8::new(UNINITIALISED);

pub const fn encode(kind: Kind) -> u8 {
    match kind {
        Kind::Qemu => TAG_QEMU,
        Kind::Sm8650 => TAG_SM8650,
        Kind::Fog => TAG_FOG,
        Kind::Bcm2712 => TAG_BCM2712,
        Kind::Unknown => TAG_UNKNOWN,
    }
}

pub const fn decode(tag: u8) -> Option<Kind> {
    match tag {
        TAG_QEMU => Some(Kind::Qemu),
        TAG_SM8650 => Some(Kind::Sm8650),
        TAG_FOG => Some(Kind::Fog),
        TAG_BCM2712 => Some(Kind::Bcm2712),
        TAG_UNKNOWN => Some(Kind::Unknown),
        _ => None,
    }
}

/// Records the identity proved by the live FDT. Called once, before the console
/// is initialised.
pub fn force(kind: Kind) {
    KIND.store(encode(kind), Ordering::Release);
}

/// The identity recorded so far, if any.
pub fn cached() -> Option<Kind> {
    decode(KIND.load(Ordering::Acquire))
}

/// Maps a `MIDR_EL1` part number to a platform identity.
///
/// Only parts that are unambiguous in this project are recognised: `0xD08` is
/// our QEMU `-cpu cortex-a72`, and the SM8650 Kryo cluster is `0xD80..=0xD82`.
/// A Cortex-A76 (`0xD0B`, Raspberry Pi 5) is deliberately *not* mapped here:
/// the same core appears in many machines, so BCM2712 must be proved from the
/// device tree instead of guessed from the CPU.
pub const fn kind_from_midr_part(part: u32) -> Kind {
    match part {
        0xD08 => Kind::Qemu,
        0xD80..=0xD82 => Kind::Sm8650,
        _ => Kind::Unknown,
    }
}

/// Maps a device-tree root `compatible` list to a platform identity.
///
/// The value is the raw FDT property: NUL-separated strings, most specific
/// first (`raspberrypi,5-model-b\0brcm,bcm2712\0...`). Each entry is matched
/// whole — a substring test would accept a vendor string that merely mentions
/// the SoC — and only identities whose peripheral map we know are returned.
pub fn kind_from_compatible(compatible: &str) -> Option<Kind> {
    for entry in compatible.split('\0') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "brcm,bcm2712" || entry.starts_with("raspberrypi,5-") {
            return Some(Kind::Bcm2712);
        }
    }
    None
}

/// Applies [`kind_from_compatible`] when it recognises the tree.
///
/// Returns the identity it forced, or `None` when the compatible list names no
/// machine this build knows. The boot path calls this once the FDT is parsed
/// and before the console is initialised; today only the `fog` path does so,
/// because no other physical board is supported (see `docs/PI5.md`).
pub fn force_from_compatible(compatible: &str) -> Option<Kind> {
    let kind = kind_from_compatible(compatible)?;
    force(kind);
    Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_raspberry_pi_5_tree() {
        let compatible = "raspberrypi,5-model-b\0brcm,bcm2712\0";
        assert_eq!(kind_from_compatible(compatible), Some(Kind::Bcm2712));
    }

    #[test]
    fn recognises_the_soc_alone() {
        assert_eq!(kind_from_compatible("brcm,bcm2712"), Some(Kind::Bcm2712));
    }

    #[test]
    fn pi_4_is_not_claimed_as_a_pi_5() {
        assert_eq!(
            kind_from_compatible("raspberrypi,4-model-b\0brcm,bcm2711\0"),
            None
        );
    }

    #[test]
    fn qemu_and_empty_trees_name_no_board() {
        assert_eq!(kind_from_compatible("linux,dummy-virt"), None);
        assert_eq!(kind_from_compatible(""), None);
        // A substring match would be a bug: the SoC must be its own entry.
        assert_eq!(kind_from_compatible("vendor,not-brcm,bcm2712-thing"), None);
    }

    #[test]
    fn bcm2712_is_not_a_gicv3_platform_and_is_a_board() {
        assert!(!Kind::Bcm2712.uses_gicv3());
        assert!(Kind::Bcm2712.is_board());
        assert!(!Kind::Qemu.is_board());
    }

    #[test]
    fn midr_mapping_is_conservative() {
        assert_eq!(kind_from_midr_part(0xD08), Kind::Qemu);
        assert_eq!(kind_from_midr_part(0xD81), Kind::Sm8650);
        // Cortex-A76: identity must come from the device tree, not from MIDR.
        assert_eq!(kind_from_midr_part(0xD0B), Kind::Unknown);
        assert_eq!(kind_from_midr_part(0), Kind::Unknown);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        for kind in [
            Kind::Qemu,
            Kind::Sm8650,
            Kind::Fog,
            Kind::Bcm2712,
            Kind::Unknown,
        ] {
            assert_eq!(decode(encode(kind)), Some(kind));
        }
        assert_eq!(decode(UNINITIALISED), None);
        assert_eq!(decode(200), None);
    }
}
