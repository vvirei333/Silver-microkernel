//! Pure cache-geometry helpers.
//!
//! The kernel reads the geometry from `CTR_EL0`, `CLIDR_EL1` and `CCSIDR_EL1`
//! and then walks sets and ways in assembly. The decoding is kept here so the
//! arithmetic (line size, way shift, set/way operand) is host-tested instead
//! of being an untested constant in the maintenance loop.

/// Bytes per data-cache line from `CTR_EL0.DminLine` (bits [19:16]).
///
/// The field is the base-2 logarithm of the line size in 4-byte words, so the
/// smallest architectural line (16 bytes) encodes 2 and a 64-byte line
/// encodes 4.
pub fn dmin_line_bytes(ctr: u64) -> usize {
    4usize << ((ctr >> 16) & 0xF)
}

/// Bytes per instruction-cache line from `CTR_EL0.IminLine` (bits [3:0]).
pub fn imin_line_bytes(ctr: u64) -> usize {
    4usize << (ctr & 0xF)
}

/// Cache type at a 1-based `CLIDR_EL1` level: 0 = none, 1 = instruction only,
/// 2 = data only, 3 = separate, 4 = unified.
pub fn clidr_type(clidr: u64, level: usize) -> u64 {
    if !(1..=7).contains(&level) {
        return 0;
    }
    (clidr >> (3 * level)) & 0b111
}

/// Whether the level holds data (types 2, 3 and 4 all do).
pub fn level_has_data_cache(clidr: u64, level: usize) -> bool {
    clidr_type(clidr, level) >= 2
}

/// Decodes `CCSIDR_EL1` into `(line_bytes, ways, sets)`.
///
/// `LineSize` is `log2(bytes) - 4`, `Associativity` stores `ways - 1` and
/// `NumSets` stores `sets - 1`. Architecturally both are powers of two; a
/// malformed value is clamped to one rather than producing a zero shift.
pub fn ccsidr_geometry(ccsidr: u64) -> (usize, usize, usize) {
    let line_bytes = 16usize << (ccsidr & 0x7);
    let ways = (((ccsidr >> 3) & 0x3FF) + 1) as usize;
    let sets = (((ccsidr >> 13) & 0x7FFF) + 1) as usize;
    (line_bytes, ways, sets)
}

/// Operand for `dc isw`: `way << way_shift | set << line_shift | level << 1`.
///
/// `level` is the 0-based CSSELR level (CLIDR level minus one); bit 0 selects
/// data (0) or instruction (1) and stays clear here.
pub fn set_way_operand(
    level: usize,
    line_shift: u32,
    way_shift: u32,
    way: usize,
    set: usize,
) -> u64 {
    ((way as u64) << way_shift) | ((set as u64) << line_shift) | ((level as u64) << 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_size_decodes_the_architectural_minimum() {
        // DminLine = 4 -> 64 bytes; IminLine = 4 -> 64 bytes.
        assert_eq!(dmin_line_bytes(0x0004_0000), 64);
        assert_eq!(dmin_line_bytes(0x0002_0000), 16);
        assert_eq!(imin_line_bytes(0x0000_0004), 64);
    }

    #[test]
    fn ccsidr_decodes_a_typical_l1() {
        // 64-byte lines, 4 ways, 64 sets.
        let ccsidr = 0x0000_0000 | (3 << 3) | (63 << 13) | 2;
        assert_eq!(ccsidr_geometry(ccsidr), (64, 4, 64));
    }

    #[test]
    fn ccsidr_decodes_a_unified_l2() {
        // 64-byte lines, 16 ways, 512 sets.
        let ccsidr = (15 << 3) | (511 << 13) | 2;
        assert_eq!(ccsidr_geometry(ccsidr), (64, 16, 512));
    }

    #[test]
    fn clidr_classifies_levels() {
        // Level 1: separate (3), level 2: unified (4), level 3: none (0).
        let clidr = (3 << 3) | (4 << 6);
        assert_eq!(clidr_type(clidr, 1), 3);
        assert_eq!(clidr_type(clidr, 2), 4);
        assert_eq!(clidr_type(clidr, 3), 0);
        assert!(level_has_data_cache(clidr, 1));
        assert!(level_has_data_cache(clidr, 2));
        assert!(!level_has_data_cache(clidr, 3));
        // Instruction-only levels (1) are not data caches.
        assert!(!level_has_data_cache(1 << 3, 1));
    }

    #[test]
    fn out_of_range_levels_are_empty() {
        assert_eq!(clidr_type(u64::MAX, 0), 0);
        assert_eq!(clidr_type(u64::MAX, 8), 0);
    }

    #[test]
    fn set_way_operand_packs_fields() {
        // Level 0, 64-byte lines (shift 6), 4 ways (way shift 30), way 1, set 2.
        assert_eq!(
            set_way_operand(0, 6, 30, 1, 2),
            (1 << 30) | (2 << 6)
        );
        // Level 2, way 0, set 0 -> just the level field.
        assert_eq!(set_way_operand(2, 6, 30, 0, 0), 4);
    }
}
