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

/// Fraction of the usable frames kept for the kernel: one sixteenth.
///
/// One eighth (31 MiB on a 256 MiB machine) left too little headroom for a
/// full Phosh session: a 5 MiB GDK shm pool was refused while the kernel
/// itself only needs a few MiB beyond its pre-reserved heap.
pub const RESERVE_DIVISOR: usize = 16;

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

/// Fraction of the usable frames a single task may charge. A GTK4 process
/// maps ~120 MiB of libraries (ICU data alone is 32 MiB) before it runs, so
/// half of a 256 MiB machine is not enough. Three quarters still leaves the
/// kernel reserve intact, and the reserve check in `user_alloc_ok` remains the
/// hard floor for every individual allocation.
pub const TASK_LIMIT_NUMERATOR: usize = 3;
pub const TASK_LIMIT_DENOMINATOR: usize = 4;

/// Floor for the per-task charge on a tiny window, so the flat spawn cost
/// (page tables, stacks, private page) still fits.
const MIN_TASK_FRAMES: usize = 16;

/// Frames a single task may have charged at once on a machine with
/// `total_frames` usable frames.
pub fn task_frame_limit(total_frames: usize) -> usize {
    (total_frames / TASK_LIMIT_DENOMINATOR * TASK_LIMIT_NUMERATOR)
        .max(MIN_TASK_FRAMES.min(total_frames))
}

/// Coarse memory-pressure level, in the spirit of iOS' memorystatus: a
/// consumer can shed caches at `Warning` before the kernel has to refuse an
/// allocation at `Critical`.
///
/// `Critical` is exactly the point where user allocations start failing (free
/// frames at or below the kernel reserve); `Warning` is twice that reserve.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemoryPressure {
    Normal = 0,
    Warning = 1,
    Critical = 2,
}

pub fn pressure_level(free: usize, total: usize) -> MemoryPressure {
    let reserve = reserve_frames(total);
    if free <= reserve {
        MemoryPressure::Critical
    } else if free <= reserve.saturating_mul(2) {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Normal
    }
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
    fn reserve_is_a_sixteenth_of_the_window() {
        assert_eq!(reserve_frames(32_768), 2_048);
        assert_eq!(reserve_frames(8_192), 512);
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
        // 8 192 total -> 512 reserved, leaving 7 680 grantable.
        assert!(user_alloc_ok(8_192, 7_680, 8_192));
        assert!(!user_alloc_ok(8_192, 7_681, 8_192));
    }

    #[test]
    fn free_at_or_below_the_reserve_refuses_everything() {
        // free == reserve leaves zero grantable frames.
        assert!(!user_alloc_ok(512, 1, 8_192));
        // free below the reserve: `checked_sub` fails and nothing is granted.
        assert!(!user_alloc_ok(511, 1, 8_192));
        // One frame above the reserve grants exactly one.
        assert!(user_alloc_ok(513, 1, 8_192));
    }

    #[test]
    fn task_limit_is_three_quarters_with_a_floor() {
        assert_eq!(task_frame_limit(32_768), 24_576);
        assert_eq!(task_frame_limit(64), 48);
        // A window smaller than the floor may use all of itself, never more.
        assert_eq!(task_frame_limit(8), 8);
        assert_eq!(task_frame_limit(0), 0);
    }

    #[test]
    fn pressure_levels_track_the_reserve() {
        // 32 768 total -> 2 048 reserve; warning up to 4 096 free.
        assert_eq!(pressure_level(32_768, 32_768), MemoryPressure::Normal);
        assert_eq!(pressure_level(4_097, 32_768), MemoryPressure::Normal);
        assert_eq!(pressure_level(4_096, 32_768), MemoryPressure::Warning);
        assert_eq!(pressure_level(2_049, 32_768), MemoryPressure::Warning);
        assert_eq!(pressure_level(2_048, 32_768), MemoryPressure::Critical);
        assert_eq!(pressure_level(0, 32_768), MemoryPressure::Critical);
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
