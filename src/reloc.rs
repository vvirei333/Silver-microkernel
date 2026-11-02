//! Pure policy for the fog boot stub's optional self-relocation.
//!
//! The assembly in `src/main.rs` mirrors these rules. This module exists so
//! the boundary decisions can be regression-tested on the host: the stub
//! cannot call Rust before the MMU is on, but it must make exactly the same
//! accept/reject choice.
//!
//! The policy is deliberately fail-closed:
//!
//! * the destination must be page-aligned and its **whole span** must fit in
//!   the mapped first DRAM gigabyte, not just its first byte;
//! * the source span must fit there too;
//! * both spans must be overflow-free;
//! * the move must be completely non-overlapping;
//! * an unrecognised ARM64 header or a wild image size is refused.
//!
//! A destination inside the mapped gigabyte is still not proof that the
//! bytes are free. That proof requires the live FDT, which is why relocation
//! is opt-in (`fog-reloc`) rather than part of the default fog build.

/// `ARM\x64` as it appears in little-endian memory at `_head + 0x38`.
pub const ARM64_MAGIC: u32 = 0x644D_5241;

/// The stub copies 8 bytes at a time; every accepted image size is a multiple
/// of this.
pub const COPY_GRANULE: u64 = 8;

/// Smallest image the stub will accept (one page).
pub const MIN_IMAGE_SIZE: u64 = 0x1000;

/// Largest image the stub will copy (64 MiB).
pub const MAX_IMAGE_SIZE: u64 = 0x0400_0000;

/// The boot page tables map exactly the first DRAM gigabyte.
pub const DRAM_START: u64 = 0x4000_0000;
pub const DRAM_END: u64 = 0x8000_0000;

/// Result of evaluating a relocation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// The stub was entered at its link address: nothing to do.
    AlreadyAtLink,
    /// A completely non-overlapping move inside the mapped DRAM window.
    Copy {
        source: u64,
        destination: u64,
        size: u64,
    },
    /// The request is unsafe or unprovable: park without touching memory.
    Park(Reject),
}

/// Why the stub refuses a relocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    BadMagic,
    ImageTooSmall,
    ImageTooLarge,
    ImageUnaligned,
    DestinationUnaligned,
    DestinationOutOfRange,
    SourceOutOfRange,
    RangeOverflow,
    Overlap,
}

/// Evaluates whether the image at `actual_pc` may be copied to `link_pa`.
///
/// `image_size` and `magic` come from the ARM64 Image header at `_head`.
pub fn plan(actual_pc: u64, link_pa: u64, image_size: u64, magic: u32) -> Decision {
    if actual_pc == link_pa {
        return Decision::AlreadyAtLink;
    }
    if magic != ARM64_MAGIC {
        return Decision::Park(Reject::BadMagic);
    }
    if image_size < MIN_IMAGE_SIZE {
        return Decision::Park(Reject::ImageTooSmall);
    }
    if image_size >= MAX_IMAGE_SIZE {
        return Decision::Park(Reject::ImageTooLarge);
    }
    if image_size % COPY_GRANULE != 0 {
        return Decision::Park(Reject::ImageUnaligned);
    }
    if link_pa % 0x1000 != 0 {
        return Decision::Park(Reject::DestinationUnaligned);
    }

    // Compute both ends before range checks so an overflowing span is
    // reported as overflow, never wrapped into a "plausible" address.
    let destination_end = match link_pa.checked_add(image_size) {
        Some(end) => end,
        None => return Decision::Park(Reject::RangeOverflow),
    };
    let source_end = match actual_pc.checked_add(image_size) {
        Some(end) => end,
        None => return Decision::Park(Reject::RangeOverflow),
    };

    // Both ends of the destination must fit, not just the start. This is the
    // case where `link_pa` is valid but `link_pa + image_size` leaves DRAM.
    if link_pa < DRAM_START || link_pa >= DRAM_END || destination_end > DRAM_END {
        return Decision::Park(Reject::DestinationOutOfRange);
    }
    if actual_pc < DRAM_START || actual_pc >= DRAM_END || source_end > DRAM_END {
        return Decision::Park(Reject::SourceOutOfRange);
    }

    // Any overlap can overwrite the running copy loop or bytes between the
    // spans that this stub cannot prove dead. Adjacent spans are fine.
    if link_pa < source_end && actual_pc < destination_end {
        return Decision::Park(Reject::Overlap);
    }

    Decision::Copy {
        source: actual_pc,
        destination: link_pa,
        size: image_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: u64 = 0x4008_0000;

    fn plan_copy(actual: u64, size: u64) -> Decision {
        plan(actual, LINK, size, ARM64_MAGIC)
    }

    #[test]
    fn at_link_needs_no_copy() {
        assert_eq!(
            plan(LINK, LINK, 0, 0),
            Decision::AlreadyAtLink,
            "the equal-address case must not depend on header fields"
        );
    }

    #[test]
    fn forward_and_backward_non_overlapping_moves_are_allowed() {
        assert_eq!(
            plan_copy(0x4040_0000, 0x10_0000),
            Decision::Copy {
                source: 0x4040_0000,
                destination: LINK,
                size: 0x10_0000,
            }
        );
        assert_eq!(
            plan_copy(0x4000_0000, 0x8_0000),
            Decision::Copy {
                source: 0x4000_0000,
                destination: LINK,
                size: 0x8_0000,
            }
        );
    }

    #[test]
    fn adjacent_spans_are_not_an_overlap() {
        // source ends exactly at the destination start.
        assert!(matches!(
            plan_copy(0x4000_0000, 0x8_0000),
            Decision::Copy { .. }
        ));
        // destination ends exactly at the source start.
        assert!(matches!(
            plan_copy(0x4018_0000, 0x10_0000),
            Decision::Copy { .. }
        ));
    }

    #[test]
    fn overlap_is_refused_in_both_directions() {
        assert_eq!(
            plan_copy(0x4000_0000, 0x10_0000),
            Decision::Park(Reject::Overlap)
        );
        assert_eq!(
            plan_copy(0x4008_1000, 0x10_0000),
            Decision::Park(Reject::Overlap)
        );
    }

    #[test]
    fn bad_magic_is_refused() {
        assert_eq!(
            plan(0x4040_0000, LINK, 0x10_0000, 0),
            Decision::Park(Reject::BadMagic)
        );
    }

    #[test]
    fn wild_image_sizes_are_refused() {
        assert_eq!(
            plan_copy(0x4040_0000, 0),
            Decision::Park(Reject::ImageTooSmall)
        );
        assert_eq!(
            plan_copy(0x4040_0000, MIN_IMAGE_SIZE - 8),
            Decision::Park(Reject::ImageTooSmall)
        );
        assert_eq!(
            plan_copy(0x4040_0000, MAX_IMAGE_SIZE),
            Decision::Park(Reject::ImageTooLarge)
        );
        assert_eq!(
            plan_copy(0x4040_0000, 0x1004),
            Decision::Park(Reject::ImageUnaligned)
        );
    }

    #[test]
    fn destination_must_be_page_aligned_and_inside_dram() {
        assert_eq!(
            plan(0x4040_0000, LINK + 8, 0x1000, ARM64_MAGIC),
            Decision::Park(Reject::DestinationUnaligned)
        );
        assert_eq!(
            plan(0x4040_0000, DRAM_START - 0x1000, 0x1000, ARM64_MAGIC),
            Decision::Park(Reject::DestinationOutOfRange)
        );
        assert_eq!(
            plan(0x4040_0000, DRAM_END, 0x1000, ARM64_MAGIC),
            Decision::Park(Reject::DestinationOutOfRange)
        );
    }

    #[test]
    fn destination_end_beyond_dram_is_refused_even_when_start_is_valid() {
        // Start is page-aligned and strictly inside DRAM, but the second
        // page of the span leaves the only mapped gigabyte.
        let destination = DRAM_END - 0x1000;
        assert!(destination >= DRAM_START && destination < DRAM_END);
        assert_eq!(
            plan(0x4000_0000, destination, 0x2000, ARM64_MAGIC),
            Decision::Park(Reject::DestinationOutOfRange)
        );
        // The same span one page lower is accepted.
        assert!(matches!(
            plan(0x4000_0000, destination - 0x1000, 0x2000, ARM64_MAGIC),
            Decision::Copy { .. }
        ));
    }

    #[test]
    fn source_must_be_inside_dram() {
        assert_eq!(
            plan(DRAM_START - 0x1000, LINK, 0x1000, ARM64_MAGIC),
            Decision::Park(Reject::SourceOutOfRange)
        );
        assert_eq!(
            plan(DRAM_END, LINK, 0x1000, ARM64_MAGIC),
            Decision::Park(Reject::SourceOutOfRange)
        );
        assert_eq!(
            plan(DRAM_END - 0x1000, LINK, 0x2000, ARM64_MAGIC),
            Decision::Park(Reject::SourceOutOfRange)
        );
    }

    #[test]
    fn overflowing_spans_are_refused() {
        assert_eq!(
            plan(u64::MAX - 0xFFF, LINK, 0x2000, ARM64_MAGIC),
            Decision::Park(Reject::RangeOverflow)
        );
        assert_eq!(
            plan(0x4040_0000, u64::MAX - 0xFFF, 0x2000, ARM64_MAGIC),
            Decision::Park(Reject::RangeOverflow)
        );
    }

    #[test]
    fn accepted_copy_moves_exactly_the_header_image_size() {
        // Every accepted copy moves exactly the header's image_size.
        match plan_copy(0x5000_0000, 0x1234_000) {
            Decision::Copy { size, .. } => assert_eq!(size, 0x1234_000),
            other => panic!("unexpected decision: {other:?}"),
        }
    }
}
