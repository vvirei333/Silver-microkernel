//! Pure SDHCI arithmetic shared by the kernel driver and host tests.
//!
//! The register protocol itself is hardware-facing; these are the two pieces
//! worth testing without a controller: the clock divisor and the capacity
//! decode from the four CSD response words.

/// Returns the SDHCI 8-bit divisor for `base_hz` and a target `clock_hz`.
///
/// SDHCI divides the base clock by `2 * divisor`. The result is clamped to
/// 1..=255, which is the 8-bit field this core writes; a controller that
/// needs the 10-bit extension is out of scope.
pub fn clock_divisor(base_hz: u32, clock_hz: u32) -> u16 {
    let target = clock_hz.max(1).saturating_mul(2);
    let divisor = base_hz
        .saturating_add(target - 1)
        .checked_div(target)
        .unwrap_or(1)
        .clamp(1, 255);
    divisor as u16
}

/// Decodes card capacity in 512-byte sectors from CSD words.
///
/// `csd[0]` is the low response word and `csd[3]` the high one, matching the
/// SDHCI `RESPONSE+0`..`RESPONSE+12` order. CSD v1 (SDSC) and v2 (SDHC/SDXC)
/// are supported; unknown structures and zero capacity fail closed.
pub fn capacity_sectors(csd: [u32; 4]) -> Option<u64> {
    let structure = (csd[3] >> 30) & 0x3;
    let sectors = match structure {
        0 => {
            let c_size = u64::from(((csd[2] & 0x3ff) << 2) | ((csd[1] >> 30) & 0x3));
            let c_size_mult = (csd[1] >> 15) & 0x7;
            let read_bl_len = (csd[2] >> 16) & 0xf;
            let bytes = (c_size + 1)
                .checked_shl(c_size_mult + 2)?
                .checked_shl(read_bl_len)?;
            bytes / 512
        }
        1 => {
            let c_size = u64::from(((csd[2] & 0x3f) << 16) | (csd[1] >> 16));
            (c_size + 1) * 1024
        }
        _ => return None,
    };
    (sectors != 0).then_some(sectors)
}

#[cfg(test)]
mod tests {
    use super::{capacity_sectors, clock_divisor};

    #[test]
    fn clock_divisor_matches_the_200_mhz_base() {
        assert_eq!(clock_divisor(200_000_000, 400_000), 250);
        assert_eq!(clock_divisor(200_000_000, 25_000_000), 4);
        assert_eq!(clock_divisor(50_000_000, 400_000), 63);
    }

    #[test]
    fn clock_divisor_clamps_and_never_divides_by_zero() {
        assert_eq!(clock_divisor(200_000_000, 0), 255);
        assert_eq!(clock_divisor(0, 400_000), 1);
    }

    #[test]
    fn decodes_a_32_gib_sdhc_csd() {
        // SanDisk 32 GB SDHC, CSD = 400e00325b590000edc87f800a400000.
        let csd = [0x0a40_0000, 0xedc8_7f80, 0x5b59_0000, 0x400e_0032];
        assert_eq!(capacity_sectors(csd), Some(60_873 * 1024));
    }

    #[test]
    fn decodes_an_sdsc_csd() {
        // v1: c_size = 0x3ff, c_size_mult = 7, read_bl_len = 9.
        // c_size = 0x3ff: CSD[73:64] = 0xff, CSD[63:62] = 0x3.
        let csd = [0, (0x3 << 30) | (7 << 15), (0xff << 0) | (9 << 16), 0];
        assert_eq!(capacity_sectors(csd), Some(524_288));
    }

    #[test]
    fn rejects_unknown_and_empty_csd() {
        assert_eq!(capacity_sectors([0; 4]), None);
        assert_eq!(capacity_sectors([0, 0, 0, 2 << 30]), None);
    }
}
