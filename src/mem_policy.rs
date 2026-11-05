//! Pure frame-budget policy for user-visible allocations.
//!
//! The kernel keeps a reserve of physical frames that only kernel-internal
//! allocations may use. User-visible memory (anonymous `mmap`, the `brk` heap,
//! DMA buffers, ELF image pages and per-task stacks) is refused once granting
//! it would eat into that reserve. The decision is a pure function of the
//! allocator's current free count, so there is no per-allocation accounting
//! that could drift after a failed path or a leaked frame.
//!
//! All user-visible allocation paths run under the kernel lock, so the check
//! and the allocation it guards are serialized against the other CPUs'
//! requests. A concurrent *kernel* allocation only makes the check stricter,
//! never looser.

/// Fraction of the usable frames kept for the kernel: one eighth.
pub const RESERVE_DIVISOR: usize = 8;

/// Frames kept out of user-visible allocations on a machine with
/// `total_frames` usable frames.
///
/// The reserve never exceeds half of RAM: a tiny window (fog) must still be
/// able to start a task, and reserving more than it can ever use would turn
/// every `mmap` request into a refusal.
pub fn reserve_frames(total_frames: usize) -> usize {
    (total_frames / RESERVE_DIVISOR).min(total_frames / 2)
}

/// Whether `requested` frames may be granted to a user-visible allocation when
/// `free` frames are currently free.
///
/// `false` covers both "would dip into the reserve" and "more than exists";
/// callers fail closed with the same error either way.
pub fn user_alloc_ok(free: usize, requested: usize, total: usize) -> bool {
    let Some(remaining) = free.checked_sub(reserve_frames(total)) else {
        return false;
    };
    remaining >= requested
}

/// Fraction of the usable frames a single task may charge: one quarter.
pub const TASK_LIMIT_DIVISOR: usize = 4;

/// Floor for the per-task charge on a tiny window, so the flat spawn cost
/// (page tables, stacks, private page) still fits.
const MIN_TASK_FRAMES: usize = 16;

/// Frames a single task may have charged at once on a machine with
/// `total_frames` usable frames.
pub fn task_frame_limit(total_frames: usize) -> usize {
    (total_frames / TASK_LIMIT_DIVISOR).max(MIN_TASK_FRAMES.min(total_frames))
}

/// Whether a task that already charged `used` frames may charge `requested`
/// more without exceeding `limit`. All-or-nothing: the caller charges nothing
/// on `false`.
pub fn task_charge_ok(used: usize, requested: usize, limit: usize) -> bool {
    used.checked_add(requested)
        .map_or(false, |total| total <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_is_an_eighth_of_the_window() {
        assert_eq!(reserve_frames(32_768), 4_096);
        assert_eq!(reserve_frames(8_192), 1_024);
    }

    #[test]
    fn reserve_never_takes_more_than_half() {
        // A tiny window would otherwise reserve everything: 4/8 rounds to 0,
        // and the half cap keeps the reserve from surpassing usable RAM.
        assert_eq!(reserve_frames(4), 0);
        assert_eq!(reserve_frames(0), 0);
        assert!(reserve_frames(2) <= 1);
    }

    #[test]
    fn request_must_fit_above_the_reserve() {
        // 8 192 total -> 1 024 reserved, leaving 7 168 grantable.
        assert!(user_alloc_ok(8_192, 7_168, 8_192));
        assert!(!user_alloc_ok(8_192, 7_169, 8_192));
    }

    #[test]
    fn free_at_or_below_the_reserve_refuses_everything() {
        // free == reserve leaves zero grantable frames.
        assert!(!user_alloc_ok(1_024, 1, 8_192));
        // free below the reserve: `checked_sub` fails and nothing is granted.
        assert!(!user_alloc_ok(1_023, 1, 8_192));
        // One frame above the reserve grants exactly one.
        assert!(user_alloc_ok(1_025, 1, 8_192));
    }

    #[test]
    fn task_limit_is_a_quarter_with_a_floor() {
        assert_eq!(task_frame_limit(32_768), 8_192);
        assert_eq!(task_frame_limit(64), 16);
        // A window smaller than the floor may use all of itself, never more.
        assert_eq!(task_frame_limit(8), 8);
        assert_eq!(task_frame_limit(0), 0);
    }

    #[test]
    fn task_charge_is_all_or_nothing() {
        assert!(task_charge_ok(0, 16, 16));
        assert!(!task_charge_ok(0, 17, 16));
        assert!(task_charge_ok(10, 6, 16));
        assert!(!task_charge_ok(10, 7, 16));
        // Already over the limit (a shrunk policy) may not charge even zero.
        assert!(!task_charge_ok(20, 0, 16));
    }

    #[test]
    fn task_charge_overflow_is_refused() {
        assert!(!task_charge_ok(usize::MAX, 1, usize::MAX));
        assert!(!task_charge_ok(usize::MAX - 1, 2, usize::MAX));
    }

    #[test]
    fn absurd_request_fails_instead_of_overflowing() {
        assert!(!user_alloc_ok(usize::MAX, usize::MAX, usize::MAX));
        assert!(!user_alloc_ok(0, usize::MAX, 0));
    }
}
