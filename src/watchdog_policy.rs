//! Pure liveness decision for the per-CPU stall watchdog.
//!
//! The kernel side owns the heartbeat counters, the exemption flags and the
//! reporting; the decision itself lives here so it can be regression-tested on
//! the host. A CPU is only declared stalled after several consecutive checks
//! without progress, which keeps a single missed tick from raising a false
//! alarm.

/// Consecutive checks without a heartbeat before a CPU is declared stalled.
pub const STALL_ROUNDS: u32 = 3;

/// Verdict of one watchdog check for one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The heartbeat advanced since the previous check.
    Alive,
    /// The heartbeat has not advanced yet, but the grace window is open.
    Waiting,
    /// No heartbeat for [`STALL_ROUNDS`] consecutive checks.
    Stalled,
}

/// Folds one check into a CPU's watchdog state.
///
/// `beat` is the current heartbeat counter; `last_beat` and `stale` are the
/// stored state from the previous check. The returned tuple is
/// `(verdict, last_beat, stale)` and must be stored back for the next check.
///
/// Progress always resets the grace window, so a CPU that stalls, recovers and
/// stalls again is reported twice rather than being considered stalled from
/// the first episode onward.
pub fn observe(beat: u64, last_beat: u64, stale: u32) -> (Liveness, u64, u32) {
    if beat != last_beat {
        (Liveness::Alive, beat, 0)
    } else {
        let stale = stale.saturating_add(1);
        let verdict = if stale >= STALL_ROUNDS {
            Liveness::Stalled
        } else {
            Liveness::Waiting
        };
        (verdict, last_beat, stale)
    }
}

#[cfg(test)]
mod tests {
    use super::{observe, Liveness, STALL_ROUNDS};

    #[test]
    fn advancing_heartbeat_is_alive() {
        assert_eq!(observe(9, 8, 2), (Liveness::Alive, 9, 0));
        // First observation of a fresh counter also counts as progress: the
        // seed value must not make every CPU look stalled on its first check.
        assert_eq!(observe(1, 0, 0), (Liveness::Alive, 1, 0));
    }

    #[test]
    fn frozen_heartbeat_stalls_after_grace() {
        // The baseline must already equal the frozen beat; otherwise the first
        // comparison counts as progress.
        let mut state = (7u64, 0u32);
        for round in 1..STALL_ROUNDS {
            let (verdict, last, stale) = observe(7, state.0, state.1);
            assert_eq!(verdict, Liveness::Waiting, "round {round}");
            state = (last, stale);
        }
        let (verdict, _, stale) = observe(7, state.0, state.1);
        assert_eq!(verdict, Liveness::Stalled);
        assert_eq!(stale, STALL_ROUNDS);
    }

    #[test]
    fn recovery_resets_the_grace_window() {
        let (verdict, last, stale) = observe(7, 7, STALL_ROUNDS);
        assert_eq!(verdict, Liveness::Stalled);
        assert_eq!(observe(8, last, stale), (Liveness::Alive, 8, 0));
        // A fresh stall episode needs the full grace window again.
        assert_eq!(observe(8, 8, 0).0, Liveness::Waiting);
    }

    #[test]
    fn stale_counter_saturates() {
        let (verdict, _, stale) = observe(5, 5, u32::MAX);
        assert_eq!(verdict, Liveness::Stalled);
        assert_eq!(stale, u32::MAX);
    }
}
