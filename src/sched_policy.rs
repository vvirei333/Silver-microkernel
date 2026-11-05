//! Pure dispatch policy for the scheduler's ready queues.
//!
//! Three priority classes, FIFO inside a class, and aging across classes.
//! Aging is counted in dispatch attempts rather than wall time: every call
//! that asks for the next task advances the age of every queued entry, and an
//! entry's effective class improves by one every [`AGING_PASSES`] attempts.
//! That bounds starvation: a low-priority task reaches the top class after at
//! most `(PRIORITY_CLASSES - 1) * AGING_PASSES` attempts, no matter how many
//! higher-class tasks are enqueued behind it.

/// Highest class (dispatched first).
pub const PRIORITY_HIGH: u8 = 0;
/// Default class for every task, including all userspace tasks.
pub const PRIORITY_NORMAL: u8 = 1;
/// Lowest class.
pub const PRIORITY_LOW: u8 = 2;
/// Number of classes (0..PRIORITY_CLASSES).
pub const PRIORITY_CLASSES: u8 = 3;
/// Dispatch attempts a queued entry spends in one class before it is
/// promoted one class.
pub const AGING_PASSES: u16 = 8;

/// Clamps an arbitrary value to a valid class. Unknown values behave like the
/// lowest class rather than wrapping into a privileged one.
pub fn normalize(priority: u8) -> u8 {
    priority.min(PRIORITY_CLASSES - 1)
}

/// Effective class after `age` waiting rounds. Lower is better; the result is
/// always a valid class.
pub fn effective(priority: u8, age: u16) -> u8 {
    let promotions = (age / AGING_PASSES).min(PRIORITY_CLASSES as u16) as u8;
    normalize(priority).saturating_sub(promotions)
}

/// Index of the entry to dispatch: the FIFO-first entry with the lowest
/// effective class. Returns `None` for an empty queue.
///
/// Strict `<` keeps the earliest entry of a class, so a tie (including one
/// produced by aging) does not reorder same-class work.
pub fn pick(entries: &[(u8, u16)]) -> Option<usize> {
    let mut best = None;
    let mut best_class = u8::MAX;
    for (position, &(priority, age)) in entries.iter().enumerate() {
        let class = effective(priority, age);
        if class < best_class {
            best = Some(position);
            best_class = class;
        }
    }
    best
}

/// Picks an idle peer to receive one queued task from `from`.
///
/// `loads[cpu]` is that CPU's queue length and `idle[cpu]` says it is
/// currently running its idle task. A push only makes sense into an empty
/// queue of an idle CPU: handing work to a peer that already has local work
/// would reorder its queue for no gain. The first suitable CPU wins, so the
/// choice is deterministic.
pub fn push_target(loads: &[usize], idle: &[bool], from: usize) -> Option<usize> {
    if loads.get(from).copied().unwrap_or(0) == 0 {
        return None;
    }
    (0..loads.len()).find(|&cpu| {
        cpu != from && idle.get(cpu).copied().unwrap_or(false) && loads[cpu] == 0
    })
}

#[cfg(test)]
mod tests {
    use super::{
        effective, normalize, pick, push_target, PRIORITY_CLASSES, PRIORITY_HIGH, PRIORITY_LOW,
        PRIORITY_NORMAL, AGING_PASSES,
    };

    #[test]
    fn normalize_clamps_to_a_valid_class() {
        assert_eq!(normalize(PRIORITY_HIGH), PRIORITY_HIGH);
        assert_eq!(normalize(PRIORITY_NORMAL), PRIORITY_NORMAL);
        assert_eq!(normalize(PRIORITY_LOW), PRIORITY_LOW);
        assert_eq!(normalize(PRIORITY_CLASSES), PRIORITY_LOW);
        assert_eq!(normalize(u8::MAX), PRIORITY_LOW);
    }

    #[test]
    fn effective_promotes_one_class_per_aging_window() {
        assert_eq!(effective(PRIORITY_LOW, 0), PRIORITY_LOW);
        assert_eq!(effective(PRIORITY_LOW, AGING_PASSES - 1), PRIORITY_LOW);
        assert_eq!(effective(PRIORITY_LOW, AGING_PASSES), PRIORITY_NORMAL);
        assert_eq!(effective(PRIORITY_LOW, AGING_PASSES * 2), PRIORITY_HIGH);
        // No promotion past the top class, and a high entry never regresses.
        assert_eq!(effective(PRIORITY_LOW, u16::MAX), PRIORITY_HIGH);
        assert_eq!(effective(PRIORITY_HIGH, u16::MAX), PRIORITY_HIGH);
    }

    #[test]
    fn pick_prefers_the_highest_class() {
        assert_eq!(pick(&[(PRIORITY_LOW, 0), (PRIORITY_HIGH, 0)]), Some(1));
        assert_eq!(pick(&[(PRIORITY_NORMAL, 0), (PRIORITY_HIGH, 0)]), Some(1));
    }

    #[test]
    fn pick_is_fifo_inside_a_class() {
        assert_eq!(pick(&[(PRIORITY_NORMAL, 0)]), Some(0));
        assert_eq!(
            pick(&[(PRIORITY_NORMAL, 0), (PRIORITY_NORMAL, 0)]),
            Some(0)
        );
        // Equal effective classes also keep the earlier entry.
        assert_eq!(
            pick(&[(PRIORITY_LOW, AGING_PASSES * 2), (PRIORITY_HIGH, 0)]),
            Some(0)
        );
    }

    #[test]
    fn aging_bounds_waiting_under_constant_pressure() {
        // The low entry reaches the top class after exactly two aging windows
        // and then wins against a fresh high-priority arrival because it sits
        // in front of it.
        let just_short = AGING_PASSES * 2 - 1;
        assert_eq!(
            pick(&[(PRIORITY_LOW, just_short), (PRIORITY_HIGH, 0)]),
            Some(1),
            "one pass before the window expires, the high entry wins"
        );
        assert_eq!(
            pick(&[(PRIORITY_LOW, AGING_PASSES * 2), (PRIORITY_HIGH, 0)]),
            Some(0),
            "after the window the aged entry is effectively high and is first"
        );
    }

    #[test]
    fn empty_queue_has_no_pick() {
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn push_needs_local_work() {
        assert_eq!(push_target(&[0, 0], &[false, true], 0), None);
    }

    #[test]
    fn push_targets_only_an_idle_empty_peer() {
        // CPU1 is idle and empty; CPU2 is idle but already has a queued task;
        // CPU3 is busy. CPU1 wins.
        assert_eq!(
            push_target(&[2, 0, 1, 0], &[false, true, true, false], 0),
            Some(1)
        );
        // No idle empty peer: do not push.
        assert_eq!(push_target(&[2, 1, 1], &[false, true, true], 0), None);
        // A CPU never pushes to itself, even if its own queue is empty.
        assert_eq!(push_target(&[1, 0], &[true, true], 1), None);
    }

    #[test]
    fn push_ignores_a_short_idle_slice() {
        // A caller may pass a shorter "idle" view than "loads"; missing entries
        // are treated as not idle rather than panicking.
        assert_eq!(push_target(&[1, 0, 0], &[false, true], 0), Some(1));
        assert_eq!(push_target(&[1, 0, 0], &[false], 0), None);
    }
}
