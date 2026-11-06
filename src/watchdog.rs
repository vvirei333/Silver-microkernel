//! Per-CPU stall watchdog.
//!
//! A CPU that is online and has entered the scheduler must keep making
//! progress: it takes its timer PPI at least once per check round, or it
//! completes a scheduling pass (which also advances the heartbeat). If neither
//! happens for [`watchdog_policy::STALL_ROUNDS`] consecutive checks, any other
//! PE reports it. This is the class of hang where a PE spins with IRQs masked
//! or sits in a non-scheduling loop: the system is up, the other CPUs are
//! healthy, but this one never runs anything again.
//!
//! Deliberate exemptions, all bounded by design:
//! * a CPU that has not entered the scheduler loop yet (AP bring-up);
//! * a stop-the-world quiesce, which parks peers with IRQs masked;
//! * a CPU spinning in `bkl_acquire`, where an unbounded wait already prints
//!   `BKL STUCK` and the duration is bounded by the current holder.
//!
//! A stalled CPU is reported once per episode (console and a counter) rather
//! than panicking: the watchdog is a diagnostic, and a PE can legitimately
//! recover after a long mask. The opt-in `smp-churn` profile proves the
//! detector with a task that really spins with IRQs masked.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use silver_microkernel::watchdog_policy::{observe, Liveness};

use crate::{console, smp, timer};

/// Check cadence in per-CPU timer interrupts (25 × 10 ms = 250 ms). All four
/// CPUs check independently, so a real stall is noticed well under a second.
const CHECK_EVERY_TICKS: u64 = 25;

/// Sticky state, one slot per possible CPU. Only a CPU that calls
/// [`cpu_started`] is ever judged; the rest of the counters stay at zero.
static WATCHED: [AtomicBool; smp::MAX_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
];
static BEAT: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
static LAST_BEAT: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
static STALE: [AtomicU32; smp::MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
AtomicU32::new(0),
AtomicU32::new(0),
AtomicU32::new(0),
AtomicU32::new(0),
];
static REPORTED: [AtomicBool; smp::MAX_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
];
static BKL_WAITING: [AtomicBool; smp::MAX_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
AtomicBool::new(false),
];

/// One checker at a time; a contended check is simply skipped because the
/// next timer tick will run another one.
static CHECKING: AtomicBool = AtomicBool::new(false);
static CHECKS: AtomicU64 = AtomicU64::new(0);
static STALLS: AtomicU64 = AtomicU64::new(0);

/// Publishes `cpu` as a watched PE. Called once, right before it enters the
/// scheduler loop, so AP bring-up and the boot selftests (which park peers with
/// IRQs masked) are never judged.
pub fn cpu_started(cpu: usize) {
    if cpu >= smp::MAX_CPUS {
        return;
    }
    // Seed the baseline with the current heartbeat: scheduler passes may
    // already have run before `start()`.
    let beat = BEAT[cpu].fetch_add(1, Ordering::Relaxed) + 1;
    LAST_BEAT[cpu].store(beat, Ordering::Relaxed);
    STALE[cpu].store(0, Ordering::Relaxed);
    REPORTED[cpu].store(false, Ordering::Relaxed);
    WATCHED[cpu].store(true, Ordering::Release);
}

/// Records scheduler progress for `cpu`. A PE that is stuck inside a pass
/// without taking interrupts still proves it is alive this way.
pub fn note_progress(cpu: usize) {
    if let Some(beat) = BEAT.get(cpu) {
        beat.fetch_add(1, Ordering::Relaxed);
    }
}

/// Marks `cpu` as parked in the kernel-lock acquire loop. Such a PE has IRQs
/// masked by construction, and `BKL STUCK` owns the diagnostic for it.
pub fn set_bkl_waiting(cpu: usize, waiting: bool) {
    if let Some(flag) = BKL_WAITING.get(cpu) {
        flag.store(waiting, Ordering::Relaxed);
    }
}

/// Timer-IRQ hook: advances the heartbeat and, once per check round, scans
/// every watched CPU.
pub fn on_timer_tick(cpu: usize) {
    note_progress(cpu);
    if !WATCHED.get(cpu).is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return;
    }
    if timer::cpu_ticks(cpu) % CHECK_EVERY_TICKS == 0 {
        check();
    }
}

fn check() {
    if CHECKING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    // A stop-the-world round parks every peer with IRQs masked on purpose.
    // Rebasing the grace window keeps the first post-quiesce check from
    // blaming the parked CPUs for the time they spent acknowledging.
    if smp::quiesce_active() {
        refresh_all();
        CHECKING.store(false, Ordering::Release);
        return;
    }

    CHECKS.fetch_add(1, Ordering::Relaxed);
    for cpu in 0..smp::MAX_CPUS {
        if !WATCHED[cpu].load(Ordering::Acquire) || !smp::cpu_online(cpu) {
            continue;
        }
        if BKL_WAITING[cpu].load(Ordering::Relaxed) {
            refresh(cpu);
            continue;
        }
        let beat = BEAT[cpu].load(Ordering::Relaxed);
        let (verdict, last, stale) = observe(
            beat,
            LAST_BEAT[cpu].load(Ordering::Relaxed),
            STALE[cpu].load(Ordering::Relaxed),
        );
        LAST_BEAT[cpu].store(last, Ordering::Relaxed);
        STALE[cpu].store(stale, Ordering::Relaxed);
        match verdict {
            Liveness::Stalled => {
                if !REPORTED[cpu].swap(true, Ordering::AcqRel) {
                    STALLS.fetch_add(1, Ordering::AcqRel);
                    report(cpu, beat, stale);
                }
            }
            Liveness::Alive => {
                REPORTED[cpu].store(false, Ordering::Relaxed);
            }
            Liveness::Waiting => {}
        }
    }
    CHECKING.store(false, Ordering::Release);
}

fn refresh(cpu: usize) {
    LAST_BEAT[cpu].store(BEAT[cpu].load(Ordering::Relaxed), Ordering::Relaxed);
    STALE[cpu].store(0, Ordering::Relaxed);
}

fn refresh_all() {
    for cpu in 0..smp::MAX_CPUS {
        refresh(cpu);
    }
}

fn report(cpu: usize, beat: u64, stale: u32) {
    console::write_str("WATCHDOG: CPU");
    console::write_dec_u64(cpu as u64);
    console::write_str(" stalled (beats=");
    console::write_dec_u64(beat);
    console::write_str(" task=");
    let task = smp::current_task_of(cpu);
    if task == 0 {
        console::write_str("none");
    } else {
        console::write_dec_u64(task);
    }
    console::write_str(" rounds=");
    console::write_dec_u64(stale as u64);
    console::write_str(")\r\n");
}

/// Checks that every online, watched **peer** CPU is still taking its timer
/// PPI. Runs after the APs have entered the scheduler loop; a frozen heartbeat
/// here is a real stall, so the answer is a boot failure rather than a log
/// line.
///
/// The calling CPU is excluded on purpose: this test runs inside a scheduling
/// pass with IRQs masked, so the caller cannot take a tick during the wait. If
/// it has not taken one yet, counting it turns the check into a race about
/// boot timing (observed as a rare `live=3/4`). The caller proves its own
/// liveness by executing the check; its PPI path is covered by the boot
/// `clock`/`ticks` tests and by the post-boot stall monitor.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_heartbeat_selftest() -> bool {
    let local = smp::this_cpu_id();
    let local_online = smp::cpu_online(local);
    let expected = smp::online_cpu_count().saturating_sub(u64::from(local_online) as usize);
    if expected == 0 {
        return true;
    }
    let mut spins: u32 = 0;
    let deadline = timer::ticks().saturating_add(300);
    loop {
        let live = (0..smp::MAX_CPUS)
            .filter(|&cpu| {
                cpu != local
                    && WATCHED[cpu].load(Ordering::Acquire)
                    && smp::cpu_online(cpu)
                    && timer::cpu_ticks(cpu) > 0
            })
            .count();
        if live >= expected {
            console::write_str("WATCHDOG: heartbeat PASS (");
            console::write_dec_u64(live as u64);
            console::write_str(" peer CPU(s) ticking)\r\n");
            return true;
        }
        spins += 1;
        if timer::ticks() >= deadline || spins >= 50_000_000 {
            console::write_str("WATCHDOG: heartbeat FAIL (live=");
            console::write_dec_u64(live as u64);
            console::write_str("/");
            console::write_dec_u64(expected as u64);
            console::write_str(")\r\n");
            return false;
        }
        core::hint::spin_loop();
    }
}

/// In-kernel sanity check of the linked decision policy. The host suite owns
/// the exhaustive cases; this proves the kernel compiled the same logic.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_policy_selftest() -> bool {
    use silver_microkernel::watchdog_policy::STALL_ROUNDS;
    let mut ok = observe(0, 0, 0).0 == Liveness::Waiting;
    let mut last = 0u64;
    let mut stale = 0u32;
    for _ in 1..STALL_ROUNDS {
        let (verdict, next_last, next_stale) = observe(0, last, stale);
        ok &= verdict == Liveness::Waiting;
        last = next_last;
        stale = next_stale;
    }
    ok &= observe(0, last, stale).0 == Liveness::Stalled;
    ok &= observe(1, last, stale).0 == Liveness::Alive;
    if ok {
        console::write_str("WATCHDOG: policy PASS\r\n");
    } else {
        console::write_str("WATCHDOG: policy FAIL\r\n");
    }
    ok
}

/// Number of stall reports since the last reset. `smp-churn` asserts this
/// becomes non-zero after a task deliberately spins with IRQs masked.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn stall_count() -> u64 {
    STALLS.load(Ordering::Acquire)
}

/// Clears the report counters and rebases every CPU's grace window. Only the
/// stall-injection test uses it, and only before it starts the probe.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn reset_for_test() {
    STALLS.store(0, Ordering::Relaxed);
    for cpu in 0..smp::MAX_CPUS {
        REPORTED[cpu].store(false, Ordering::Relaxed);
        refresh(cpu);
    }
}

#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn checks() -> u64 {
    CHECKS.load(Ordering::Acquire)
}
