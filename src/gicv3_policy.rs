//! Pure policy for locating this CPU's GICv3 redistributor and for the
//! bounded readback checks used during init.
//!
//! The MMIO access stays in `src/gicv3.rs`; this module only owns the
//! affinity-matching and wait/readback decisions so they can be host-tested.
//! The runtime must never guess "frame 0" when no `GICR_TYPER` matches the
//! CPU: enabling a PPI on another PE's redistributor silently drops the timer
//! interrupt.

/// Packed Aff3 << 24 | Aff2 << 16 | Aff1 << 8 | Aff0. In `GICR_TYPER` this
/// value lives in bits [63:32], not in the low word: an implementation puts
/// `Processor_Number`, `Last` and `DirectLPI` in bits [31:0].
pub const AFFINITY_MASK: u64 = 0xFFFF_FFFF;
pub const AFFINITY_SHIFT: u32 = 32;

/// Extracts the affinity value carried by a `GICR_TYPER` word.
pub fn typer_affinity(typer: u64) -> u64 {
    (typer >> AFFINITY_SHIFT) & AFFINITY_MASK
}

/// Packs `MPIDR_EL1` into the same layout as [`typer_affinity`].
///
/// MPIDR is not contiguous: Aff3 occupies bits [39:32], while Aff2/Aff1/Aff0
/// occupy [23:16]/[15:8]/[7:0]. Bit 31 is the U flag and must never be
/// compared against a redistributor affinity.
pub fn mpidr_affinity(mpidr: u64) -> u64 {
    let aff0 = mpidr & 0xFF;
    let aff1 = (mpidr >> 8) & 0xFF;
    let aff2 = (mpidr >> 16) & 0xFF;
    let aff3 = (mpidr >> 32) & 0xFF;
    (aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0
}

/// `GICR_TYPER` bit 4: the last implemented redistributor in the region.
/// Reading past it can be an external abort where frames are not populated.
pub const TYPER_LAST: u64 = 1 << 4;

/// Returns the frame index whose `GICR_TYPER` affinity matches `affinity`.
///
/// `typer_at(index)` reads the 64-bit type register of frame `index`. The
/// search stops after the frame marked LAST and returns `None` when no frame
/// matches, so the caller can fail closed instead of using frame 0.
pub fn find_redistributor_index<F>(
    affinity: u64,
    max_frames: usize,
    mut typer_at: F,
) -> Option<usize>
where
    F: FnMut(usize) -> u64,
{
    let affinity = affinity & AFFINITY_MASK;
    for index in 0..max_frames {
        let typer = typer_at(index);
        if typer_affinity(typer) == affinity {
            return Some(index);
        }
        if typer & TYPER_LAST != 0 {
            break;
        }
    }
    None
}

/// Returns true when every bit in `required` is set in `value`.
///
/// `required == 0` is rejected: an empty requirement would otherwise report
/// success without proving anything about the register.
pub fn required_bits_set(value: u32, required: u32) -> bool {
    required != 0 && value & required == required
}

/// Polls `ready` until it returns true, at most `limit` retries after the
/// first read. Returns false on timeout.
///
/// This bounds `GICR_WAKER.ChildrenAsleep` and `GICD_CTLR.RWP`: a missing
/// clock or an unpopulated frame must end the boot with `TIMER_READY=false`
/// instead of hanging forever in a spin loop.
pub fn bounded_wait<F>(mut ready: F, limit: u32) -> bool
where
    F: FnMut() -> bool,
{
    let mut retries = 0;
    while !ready() {
        if retries >= limit {
            return false;
        }
        retries += 1;
        core::hint::spin_loop();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typer(affinity: u64, last: bool) -> u64 {
        ((affinity & AFFINITY_MASK) << AFFINITY_SHIFT) | if last { TYPER_LAST } else { 0 }
    }

    #[test]
    fn finds_a_matching_frame_that_is_not_frame_zero() {
        let frames = [typer(0x000, false), typer(0x001, false), typer(0x100, true)];
        assert_eq!(
            find_redistributor_index(0x100, frames.len(), |index| frames[index]),
            Some(2)
        );
    }

    #[test]
    fn matches_affinity_not_frame_index() {
        // MPIDR Aff1=1 (0x100) must select the frame carrying that value even
        // when it is the first frame.
        let frames = [typer(0x100, false), typer(0x000, true)];
        assert_eq!(
            find_redistributor_index(0x100, frames.len(), |index| frames[index]),
            Some(0)
        );
    }

    #[test]
    fn absence_returns_none_instead_of_frame_zero() {
        let frames = [typer(0x000, false), typer(0x001, true)];
        assert_eq!(
            find_redistributor_index(0x200, frames.len(), |index| frames[index]),
            None
        );
    }

    #[test]
    fn search_does_not_read_past_the_last_frame() {
        let frames = [typer(0x000, true)];
        let mut highest = 0usize;
        let found = find_redistributor_index(0x123, 8, |index| {
            highest = highest.max(index);
            assert!(index < frames.len(), "read past GICR_TYPER.LAST");
            frames[index]
        });
        assert_eq!(found, None);
        assert_eq!(highest, 0);
    }

    #[test]
    fn a_last_frame_can_itself_match() {
        let frames = [typer(0x000, false), typer(0x002, true)];
        assert_eq!(
            find_redistributor_index(0x002, frames.len(), |index| frames[index]),
            Some(1)
        );
    }

    #[test]
    fn empty_region_returns_none() {
        assert_eq!(find_redistributor_index(0, 0, |_| 0), None);
    }

    #[test]
    fn mpidr_packing_ignores_the_uniprocessor_bit_and_places_aff3() {
        // QEMU boot CPU: MPIDR = U | Aff0=0. The comparison value is 0.
        assert_eq!(mpidr_affinity(0x8000_0000), 0);
        // Aff3 is at MPIDR bit 32 but at packed bit 24.
        assert_eq!(
            mpidr_affinity((1 << 32) | (2 << 16) | (3 << 8) | 4),
            0x0102_0304
        );
        assert_eq!(
            typer_affinity(0x0102_0304_0000_0000),
            0x0102_0304
        );
    }

    #[test]
    fn required_bits_must_all_be_present() {
        let required = 0b1010;
        assert!(required_bits_set(0b1010, required));
        assert!(required_bits_set(0b1111, required));
        assert!(!required_bits_set(0b0010, required));
        assert!(!required_bits_set(0, required));
        // An empty requirement is not proof of readiness.
        assert!(!required_bits_set(0, 0));
    }

    #[test]
    fn bounded_wait_returns_immediately_when_already_ready() {
        let mut calls = 0;
        assert!(bounded_wait(
            || {
                calls += 1;
                true
            },
            4
        ));
        assert_eq!(calls, 1);
    }

    #[test]
    fn bounded_wait_succeeds_before_the_limit() {
        let mut calls = 0;
        let ready = bounded_wait(
            || {
                calls += 1;
                calls == 3
            },
            4,
        );
        assert!(ready);
        assert_eq!(calls, 3);
    }

    #[test]
    fn bounded_wait_fails_after_the_limit() {
        let mut calls = 0;
        let ready = bounded_wait(
            || {
                calls += 1;
                false
            },
            4,
        );
        assert!(!ready);
        // One initial read plus `limit` retries, then fail closed.
        assert_eq!(calls, 5);
    }

    #[test]
    fn bounded_wait_zero_limit_still_reads_once() {
        let mut calls = 0;
        assert!(!bounded_wait(
            || {
                calls += 1;
                false
            },
            0
        ));
        assert_eq!(calls, 1);
    }
}
