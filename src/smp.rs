//! SMP foundations: spinlocks, the per-CPU area, the kernel (big) lock and
//! the CPU inventory.
//!
//! ## Locking model (documented contract)
//!
//! The kernel serializes all shared kernel state on one **kernel lock (BKL)**:
//! a task executing kernel code (syscall) holds it, a blocked/ready task does
//! not. The BKL is a spinlock with per-CPU reentrancy depth (see
//! [`bkl_acquire`]/[`bkl_release`]): nested acquisition on the holding CPU
//! increments the depth, release decrements it and frees the lock at zero.
//!
//! Hold and release points:
//! - `silver_handle_syscall` acquires on entry and releases before the `eret`
//!   back to EL0 — user mode never holds kernel locks;
//! - every blocking path (`block_current_on*`, `sleep`, `wait_*`, `yield`)
//!   releases before calling [`scheduler::schedule`] and re-acquires after
//!   resuming, so a sleeping task never pins the lock and another CPU can
//!   wake tasks in the meantime;
//! - `schedule` acquires on entry; if it performs a context switch, the
//!   outgoing task is suspended *inside* `schedule` (before its release) and
//!   the incoming task resumes inside *its own* suspended `schedule` and
//!   completes the release — the lock is held by the CPU continuously across
//!   the switch without any transfer protocol;
//! - IRQ handlers acquire and release around every scheduler access.
//!
//! Because kernel mode runs with IRQs masked (the SVC/IRQ trampolines do
//! `msr daifset, #2`), an IRQ handler can never spin on a BKL held by the
//! interrupted context of the same CPU: the holder is always either a task
//! that makes progress or a handler on another CPU that will release.
//!
//! **Lock ordering:** BKL is the outermost lock. The inner spinlocks
//! (frame allocator, page-table registry) are only ever taken while the BKL
//! is held — no code path takes the BKL while holding an inner lock, so no
//! ordering cycle exists.
//!
//! IRQ masking (`save_and_disable_irq`) remains in use everywhere as local
//! belt-and-braces; it is explicitly NOT a cross-CPU synchronization.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};

use crate::{interrupts, platform};

/// Maximum CPUs the per-CPU array covers. The QEMU profile runs `-smp 2`;
/// the tablet boots a single core (ABL parks the rest).
pub const MAX_CPUS: usize = 4;

const BKL_FREE: isize = -1;

/// Per-CPU state. Address is installed in `TPIDR_EL1`; `this_cpu()` reads it.
///
/// `current_index` is the index of the task executing (or last selected) on
/// this CPU — the former global `TaskManager.current_index`, split per CPU so
/// two CPUs never share one "current task". `usize::MAX` means "none yet".
pub struct PerCpu {
    /// Exclusive claim bit: protects field publication before `online`.
    claimed: AtomicBool,
    /// Slot claimed during CPU bring-up; `false` = offline.
    pub online: AtomicBool,
    /// Slot index in [`CPUS`] (also the IPI/log identity).
    pub cpu_id: usize,
    /// This CPU's MPIDR_EL1 affinity value (for GICR matching and logs).
    pub mpidr: u64,
    /// Index into `TaskManager.tasks` of this CPU's current task.
    current_index: UnsafeCell<usize>,
    /// Task id of this CPU's current task (`0` = none). `current_index` is
    /// rewritten only under the kernel lock, but the BKL stall diagnostic has
    /// to read a peer's identity while the lock is held by someone else: this
    /// mirrored atomic is what it reads instead of walking the scheduler
    /// vector from a racing PE.
    current_task: AtomicU64,
    /// Kernel-lock nesting depth on this CPU (0 = not held here). Atomic so a
    /// diagnostic on another PE can read it without a data race; only the
    /// owning CPU mutates it.
    bkl_depth: AtomicU32,
}

unsafe impl Sync for PerCpu {}

impl PerCpu {
    const fn empty() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            online: AtomicBool::new(false),
            cpu_id: 0,
            mpidr: 0,
            current_index: UnsafeCell::new(usize::MAX),
            current_task: AtomicU64::new(0),
            bkl_depth: AtomicU32::new(0),
        }
    }
}

static CPUS: [PerCpu; MAX_CPUS] = [
    PerCpu::empty(),
    PerCpu::empty(),
    PerCpu::empty(),
    PerCpu::empty(),
];

/// CPU slots the machine actually exposes, learned from the live FDT before
/// the scheduler or AP bring-up runs. Kept at [`MAX_CPUS`] until then, and
/// clamped again on read, so a missing or malformed tree keeps the old
/// "try every slot, failed `CPU_ON` leaves it offline" behaviour.
static POSSIBLE_CPUS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(MAX_CPUS);

/// Publishes the FDT-discovered CPU count. A zero (no `/cpus` node) is
/// ignored: the platform default stays in place rather than turning the
/// machine single-core by accident.
pub fn set_possible_cpus(count: usize) {
    if count > 0 {
        POSSIBLE_CPUS.store(count.min(MAX_CPUS), Ordering::Release);
    }
}

/// Number of CPU slots the platform may bring up. QEMU `virt` reports its
/// actual `-smp` count from the FDT; a machine with fewer cores no longer
/// pretends to have [`MAX_CPUS`] slots. Both real-hardware profiles stay
/// single-core: their boot stub parks secondaries and AP bring-up is
/// QEMU-only.
pub fn cpu_count() -> usize {
    #[cfg(feature = "smp")]
    {
        if platform::detect().is_tablet() || platform::detect().is_fog() {
            1
        } else {
            POSSIBLE_CPUS.load(Ordering::Acquire).clamp(1, MAX_CPUS)
        }
    }
    #[cfg(not(feature = "smp"))]
    {
        let _ = platform::detect();
        1
    }
}

/// Number of CPUs that have published themselves online. Used by policies
/// that must reflect the machine that actually came up (for example the churn
/// harness's "enough CPUs took part" rule) rather than the possible-slot count.
pub fn online_cpu_count() -> usize {
    let mut online = 0;
    for cpu in 0..MAX_CPUS {
        if cpu_online(cpu) {
            online += 1;
        }
    }
    online
}

/// Reads this CPU's [`PerCpu`] from `TPIDR_EL1`. Must only run after
/// [`init_cpu`] installed the pointer on this CPU.
#[inline]
pub fn this_cpu() -> &'static PerCpu {
    let pointer: usize;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) pointer, options(nostack));
    }
    unsafe { &*(pointer as *const PerCpu) }
}

#[inline]
pub fn this_cpu_id() -> usize {
    this_cpu().cpu_id
}

#[inline]
pub fn cpu_online(cpu_id: usize) -> bool {
    CPUS[cpu_id].online.load(Ordering::Acquire)
}

#[inline]
pub fn current_index() -> usize {
    unsafe { *this_cpu().current_index.get() }
}

/// Only called with the kernel lock held (schedule/spawn paths).
/// Publishes both halves of "this CPU's current task": the scheduler's index
/// and the lock-free task id mirror read by the BKL stall diagnostic.
#[inline]
pub fn set_current_task(index: usize, id: u64) {
    let cpu = this_cpu();
    unsafe {
        *cpu.current_index.get() = index;
    }
    cpu.current_task.store(id, Ordering::Release);
}

/// [`set_current_task`] for another CPU; the caller holds the kernel lock.
#[inline]
pub fn set_current_task_for(cpu: usize, index: usize, id: u64) {
    unsafe {
        *CPUS[cpu].current_index.get() = index;
    }
    CPUS[cpu].current_task.store(id, Ordering::Release);
}

/// Task id dispatched on `cpu`, or `0` when it has none. Safe to call from any
/// PE: the scheduler publishes it atomically.
#[inline]
pub fn current_task_of(cpu: usize) -> u64 {
    CPUS[cpu.min(MAX_CPUS - 1)].current_task.load(Ordering::Acquire)
}

/// Reads another (online) CPU's current index — used by reap to reposition
/// every CPU after the task vector shrinks. Kernel lock must be held.
#[inline]
pub fn current_index_of(cpu: usize) -> usize {
    unsafe { *CPUS[cpu].current_index.get() }
}

/// Installs the per-CPU pointer and claims `slot`. Runs with MMU state as the
/// caller has it (the register write itself needs no mapping). Also stores
/// the CPU id and MPIDR for later IPI/GICR use.
pub fn init_cpu(slot: usize) {
    init_cpu_unpublished(slot);
    publish_cpu(slot);
}

/// Installs the per-CPU pointer and writes this CPU's identity, but leaves
/// the slot offline. APs use this so they are not visible to IPI/quiescence
/// before their banked GIC state is proven usable.
pub fn init_cpu_unpublished(slot: usize) {
    let cpu = &CPUS[slot];
    cpu.claimed.store(true, Ordering::Release);
    // Writes race with nothing: the slot is not yet published (`online` is
    // set later with Release), and no other CPU reads this slot's fields.
    unsafe {
        let cpu = cpu as *const PerCpu as *mut PerCpu;
        (*cpu).cpu_id = slot;
        (*cpu).mpidr = read_mpidr();
    }
    unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) cpu as *const PerCpu as usize, options(nostack));
    }
}

/// Publishes a fully initialized slot. Fields written by
/// [`init_cpu_unpublished`] are visible to any reader that observes this
/// Release store.
pub fn publish_cpu(slot: usize) {
    CPUS[slot].online.store(true, Ordering::Release);
}

fn read_mpidr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, mpidr_el1", out(reg) value, options(nostack));
    }
    value
}

/// Claims the first offline slot and installs this CPU's pointer. Used by
/// APs racing at bring-up. Returns the claimed slot, or `None` when all
/// [`MAX_CPUS`] slots are taken.
pub fn claim_cpu_slot() -> Option<usize> {
    for (slot, cpu) in CPUS.iter().enumerate() {
        if !cpu.claimed.swap(true, Ordering::AcqRel) {
            // The swap is the exclusive handoff. The caller must fill the
            // slot via `init_cpu_unpublished` and only then publish it.
            return Some(slot);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The kernel (big) lock
// ---------------------------------------------------------------------------

struct KernelLock {
    /// Holding CPU id, or [`BKL_FREE`].
    owner: AtomicIsize,
}

static KERNEL_LOCK: KernelLock = KernelLock {
    owner: AtomicIsize::new(BKL_FREE),
};

/// BISECT switch: 1 = kernel lock disabled entirely.
pub static BKL_DISABLED: AtomicBool = AtomicBool::new(false);

/// A waiter that has not acquired the kernel lock for this many ticks reports
/// the holder. At 100 Hz this is one second: long enough that a normal
/// syscall or scheduling pass never trips it, short enough to catch a hold
/// that will never end. The report is diagnostic only — stealing a live lock
/// is unsafe, so there is no recovery.
const BKL_STALL_TICKS: u64 = 100;

/// Fallback for the window before global time is published (the system
/// counter is not yet the tick source): 50M yields is already pathological.
const BKL_SPIN_REPORT_LIMIT: u32 = 50_000_000;

static BKL_STALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Best-effort serialization for the report line. Every waiter reports without
/// the lock, so two peers can reach the console at the same time and interleave
/// their fields into an unreadable line. The counter still counts every
/// episode; only the printing is single-flight.
static BKL_REPORTING: AtomicBool = AtomicBool::new(false);

/// One report per waiter episode. Cleared when the lock is finally acquired,
/// so a later independent stall is reported again instead of being masked by
/// the first one.
static BKL_STALL_REPORTED: [AtomicBool; MAX_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

/// Keeps the intentional selftest stall out of normal boot logs; the counter
/// still increments and is what the test asserts on.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static BKL_STALL_SILENT: AtomicBool = AtomicBool::new(false);

/// Number of kernel-lock stall reports since boot. A non-zero value on a
/// production boot is a serious scheduling/lock bug worth collecting.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn bkl_stalls() -> u64 {
    BKL_STALLS.load(Ordering::Acquire)
}

/// Enables AP task execution once the per-CPU idle contexts exist. Kept as a
/// runtime switch for bisect/rollback; `smp` builds default to true.
#[cfg(feature = "smp")]
pub static AP_TASK_SCHEDULING: AtomicBool = AtomicBool::new(true);

/// CPUs PSCI accepted (`CPU_ON` returned success) plus the boot CPU. The
/// self-test waits for exactly this many CPUs, so a `-smp 2` machine does not
/// block on slots the firmware rejected while `-smp 4` waits for all three APs.
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;

#[cfg(feature = "smp")]
static EXPECTED_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Number of CPUs the firmware accepted during bring-up (including the boot
/// CPU). Never exceeds [`cpu_count`].
#[cfg(feature = "smp-selftest")]
pub fn expected_cpus() -> usize {
    EXPECTED_CPUS.load(Ordering::Acquire)
}

/// Debug probe chain (read via QEMU monitor `xp` at its physical address,
/// which equals the identity-mapped VA). Stages: 1=wait entered, 2=irq saved,
/// 3=guard dropped, 4=block_current_on entered, 5=schedule entered,
/// 6=schedule switched, 7=block returned.
#[no_mangle]
pub static SMP_PROBE: smp_probe_cell::ProbeCell =
    smp_probe_cell::ProbeCell(core::cell::UnsafeCell::new(0));

/// Total timer IRQ count observed (volatile; read via monitor `xp` at +4).
#[no_mangle]
pub static TICK_PROBE: smp_probe_cell::ProbeCell =
    smp_probe_cell::ProbeCell(core::cell::UnsafeCell::new(0));

/// P1 diagnostic: SPSR value saved by the SVC trampoline for the most recent
/// syscall — i.e. the user task's PSTATE at the moment it entered EL1.
/// DAIF.I is bit 7 (0x80). Read via monitor `xp` at its physical address.
#[no_mangle]
pub static USER_PSR: smp_probe_cell::ProbeCell =
    smp_probe_cell::ProbeCell(core::cell::UnsafeCell::new(0));

/// P3 diagnostic snapshot (volatile; read via monitor `xp`):
/// 0=ISR_EL1, 1=CNTP_CTL, 2=GIC pending(PPI), 3=GIC enable(PPI),
/// 4=CPU interface CTLR, 5=CPU interface PMR, 6=MPIDR(low),
/// 7=completion marker 0xD14A6001 (written LAST).
#[no_mangle]
pub static DIAG_SNAP: smp_probe_cell::ProbeArray =
    smp_probe_cell::ProbeArray(core::cell::UnsafeCell::new([0; 8]));

pub mod smp_probe_cell {
    use core::cell::UnsafeCell;
    #[repr(transparent)]
    pub struct ProbeCell(pub UnsafeCell<u32>);
    #[repr(transparent)]
    pub struct ProbeArray(pub UnsafeCell<[u32; 8]>);
    unsafe impl Sync for ProbeArray {}
    impl ProbeArray {
        pub fn store_volatile(&self, index: usize, value: u32) {
            unsafe {
                let base = self.0.get() as *mut u32;
                core::ptr::write_volatile(base.add(index), value);
            }
        }
    }
    impl ProbeCell {
        pub fn get(&self) -> *mut u32 {
            self.0.get()
        }
        /// Volatile store for P1 (low 32 bits of the saved SPSR).
        pub fn store_volatile(&self, value: u32) {
            unsafe {
                core::ptr::write_volatile(self.0.get(), value);
            }
        }

        /// Volatile increment for tick-count debugging.
        pub fn bump(&self) {
            unsafe {
                core::ptr::write_volatile(
                    self.0.get(),
                    core::ptr::read_volatile(self.0.get()).wrapping_add(1),
                );
            }
        }
    }
    unsafe impl Sync for ProbeCell {}
}

/// Volatile probe write: ordered against the volatile UART ops, unlike the
/// Relaxed atomics the optimizer was free to sink past the hang point.
#[inline]
pub fn probe(stage: u32) {
    unsafe {
        core::ptr::write_volatile(SMP_PROBE.0.get(), stage);
    }
}

/// Acquires the kernel lock, nesting on the holding CPU.
///
/// The reentrancy check reads only this CPU's own depth, which is safe: the
/// CPU that holds the lock is the only CPU that can be executing this code
/// path with a non-zero depth.
#[inline]
pub fn bkl_acquire() {
    if crate::smp::BKL_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    // The ownership store and the depth update must be one step as seen by
    // this CPU's IRQ handlers: between the CAS that publishes the owner and
    // `*depth = 1` the lock looks held (`owner == this CPU`) but depth is
    // still 0. An IRQ taken in that window would see `irq_may_schedule()`
    // and re-enter `schedule`, whose acquire can never succeed because the
    // owner is this CPU. Mask IRQs across the acquisition; callers that want
    // them enabled get their state back on return.
    let saved_daif = interrupts::save_and_disable_irq();
    let cpu = this_cpu();
    let depth = cpu.bkl_depth.load(Ordering::Relaxed);
    if depth > 0 {
        // Nesting is only ever performed by the owning CPU, so a relaxed
        // read-modify-write is enough; peers only read the value.
        cpu.bkl_depth.store(depth + 1, Ordering::Relaxed);
        interrupts::restore_irq(saved_daif);
        return;
    }
    if quiesce_active() && KERNEL_LOCK.owner.load(Ordering::Relaxed) != cpu.cpu_id as isize {
        // The quiescing CPU owns the BKL. Kernel code cannot take SGI1 with
        // IRQs masked, so acknowledge from the BKL wait loop instead.
        quiesce_ack_and_wait();
    }
    // The retry loop must be opaque to the optimizer: an empty-body
    // `while cmpxchg { spin_loop() }` was lowered by LLVM to a bare `b .`
    // (the atomic retry got deleted with the loop), hanging the kernel on
    // the first contended acquire. The asm `yield` makes the loop real.
    #[cfg(feature = "smp")]
    let mut spins: u32 = 0;
    #[cfg(feature = "smp")]
    let mut wait_start = crate::timer::ticks();
    loop {
        #[cfg(feature = "smp")]
        if quiesce_active() && KERNEL_LOCK.owner.load(Ordering::Relaxed) != cpu.cpu_id as isize {
            quiesce_ack_and_wait();
            // Quiesce parking is deliberate: restart the stall window instead
            // of counting a stop-the-world round as a stuck holder.
            wait_start = crate::timer::ticks();
        }
        let matched = KERNEL_LOCK
            .owner
            .compare_exchange_weak(
                BKL_FREE,
                cpu.cpu_id as isize,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok();
        if matched {
            break;
        }
        #[cfg(feature = "smp")]
        {
            spins = spins.saturating_add(1);
            if spins & 0x3FF == 0 {
                let waited = crate::timer::ticks().saturating_sub(wait_start);
                let stalled = waited >= BKL_STALL_TICKS || spins >= BKL_SPIN_REPORT_LIMIT;
                if stalled
                    && !BKL_STALL_REPORTED[cpu.cpu_id as usize]
                        .swap(true, Ordering::AcqRel)
                {
                    BKL_STALLS.fetch_add(1, Ordering::AcqRel);
                    report_bkl_stall(cpu.cpu_id as usize, depth, waited, spins);
                }
            }
            // This PE is masked and parked on the lock by design; the stall
            // watchdog must not double-report a bounded wait as a dead CPU.
            crate::watchdog::set_bkl_waiting(cpu.cpu_id as usize, true);
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    cpu.bkl_depth.store(1, Ordering::Relaxed);
    #[cfg(feature = "smp")]
    {
        BKL_STALL_REPORTED[cpu.cpu_id as usize].store(false, Ordering::Release);
        crate::watchdog::set_bkl_waiting(cpu.cpu_id as usize, false);
    }
    interrupts::restore_irq(saved_daif);
}

/// Reports a holder that a waiter has failed to acquire for too long. Runs on
/// the waiter with IRQs masked and the lock NOT held, so it reads the holder's
/// state without the protection of the lock: the fields it prints are plain
/// atomics/cell reads meant for diagnostics, and a torn read here is only a
/// misleading log line, never a decision.
#[cfg(feature = "smp")]
fn report_bkl_stall(waiter: usize, waiter_depth: u32, waited: u64, spins: u32) {
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    if BKL_STALL_SILENT.load(Ordering::Relaxed) {
        return;
    }
    if BKL_REPORTING.swap(true, Ordering::Acquire) {
        return;
    }
    let owner = KERNEL_LOCK.owner.load(Ordering::Relaxed);
    crate::console::write_str("BKL STALL: owner=");
    if owner < 0 {
        crate::console::write_str("free");
    } else {
        crate::console::write_dec_u64(owner as u64);
    }
    crate::console::write_str(" depth=");
    if owner >= 0 && (owner as usize) < MAX_CPUS {
        crate::console::write_dec_u64(CPUS[owner as usize].bkl_depth.load(Ordering::Relaxed) as u64);
    } else {
        crate::console::write_str("?");
    }
    crate::console::write_str(" waiter=");
    crate::console::write_dec_u64(waiter as u64);
    crate::console::write_str(" waiter_depth=");
    crate::console::write_dec_u64(waiter_depth as u64);
    crate::console::write_str(" waited=");
    crate::console::write_dec_u64(waited);
    crate::console::write_str(" spins=");
    crate::console::write_dec_u64(spins as u64);
    crate::console::write_str(" owner_task=");
    if owner >= 0 && (owner as usize) < MAX_CPUS {
        crate::console::write_dec_u64(current_task_of(owner as usize));
    } else {
        crate::console::write_str("none");
    }
    crate::console::write_str("\r\n");
    BKL_REPORTING.store(false, Ordering::Release);
}

/// Holds the kernel lock just past the stall threshold while the APs are
/// blocked in `bkl_acquire`, and requires their detector to count at least one
/// report. The hold is intentional; the report itself is silenced so ordinary
/// `smp-selftest` logs do not contain a fake failure.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_bkl_stall_selftest() -> bool {
    if cpu_count() <= 1 {
        return true;
    }
    // Start from a clean slate: an AP may have reported while it was already
    // parked on the lock during an earlier boot phase.
    BKL_STALLS.store(0, Ordering::Relaxed);
    for flag in &BKL_STALL_REPORTED {
        flag.store(false, Ordering::Relaxed);
    }
    let before = bkl_stalls();
    BKL_STALL_SILENT.store(true, Ordering::Release);
    let start = crate::timer::ticks();
    while crate::timer::ticks().saturating_sub(start) < BKL_STALL_TICKS + 30 {
        core::hint::spin_loop();
    }
    let after = bkl_stalls();
    BKL_STALL_SILENT.store(false, Ordering::Release);
    let ok = after > before;
    if ok {
        crate::console::write_str("SMP SELFTEST: bkl stall PASS (");
        crate::console::write_dec_u64(after - before);
        crate::console::write_str(" waiter report(s))\r\n");
    } else {
        crate::console::write_str("SMP SELFTEST: bkl stall FAIL (reports=");
        crate::console::write_dec_u64(after - before);
        crate::console::write_str(")\r\n");
    }
    ok
}

/// Releases the kernel lock. Must be paired with [`bkl_acquire`].
#[inline]
pub fn bkl_release() {
    if crate::smp::BKL_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    let cpu = this_cpu();
    let depth = cpu.bkl_depth.load(Ordering::Relaxed);
    debug_assert!(depth > 0, "bkl_release without acquire");
    if depth == 0 {
        // A release without a matching acquire is never legitimate: the
        // per-CPU depth counts this CPU's own holds only. Decrementing here
        // would wrap to `u32::MAX` and turn every later acquire into a silent
        // recursive no-op, so the lock would stop protecting anything. Fail
        // safe: report once and drop the release.
        static UNDERFLOW_REPORTED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !UNDERFLOW_REPORTED.swap(true, Ordering::AcqRel) {
            crate::console::write_str("BKL: release without acquire on cpu=");
            crate::console::write_dec_u64(cpu.cpu_id as u64);
            crate::console::write_str("\r\n");
        }
        return;
    }
    cpu.bkl_depth.store(depth - 1, Ordering::Relaxed);
    if depth == 1 {
        KERNEL_LOCK.owner.store(BKL_FREE, Ordering::Release);
    }
}

/// Debug aid: documents that a mutation site relies on the caller holding the
/// kernel lock (no-op outside debug builds).
#[inline]
pub fn bkl_assert_held() {
    debug_assert!(
        this_cpu().bkl_depth.load(Ordering::Relaxed) > 0,
        "kernel lock not held"
    );
}

/// RAII kernel-lock holder for functions with early returns: acquires on
/// creation, releases on drop. Functions that block (call
/// `scheduler::schedule`) must NOT use this — they follow the explicit
/// release-before-schedule / re-acquire-after-resume protocol instead.
pub struct BklGuard;

impl BklGuard {
    pub fn new() -> Self {
        bkl_acquire();
        BklGuard
    }
}

impl Default for BklGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BklGuard {
    fn drop(&mut self) {
        bkl_release();
    }
}

// ---------------------------------------------------------------------------
// Spinlock<T> for inner structures (frame allocator, page-table registry)
// ---------------------------------------------------------------------------

/// A ticket-less test-and-set spinlock guarding one inner kernel structure.
///
/// Locking disables IRQs on the local CPU for the critical section, so an
/// IRQ handler taking the same lock can never deadlock against the interrupted
/// context. Inner locks are only ever acquired while the BKL is held (see the
/// module-level lock ordering); they never call back into the scheduler.
pub struct SpinLock<T> {
    flag: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            flag: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let saved_daif = interrupts::save_and_disable_irq();
        // Opaque asm retry loop — see `bkl_acquire` for why the plain
        // `while swap { spin_loop() }` form is unsound here: LLVM deleted
        // the atomic retry and emitted an infinite `b .`.
        unsafe {
            core::arch::asm!(
                "1: ldaxr {tmp:w}, [{flag}]",
                "   cbnz {tmp:w}, 1b",
                "   mov {one:w}, #1",
                "   stlxr {tmp:w}, {one:w}, [{flag}]",
                "   cbnz {tmp:w}, 1b",
                tmp = out(reg) _,
                one = out(reg) _,
                flag = in(reg) &self.flag as *const AtomicBool as usize,
                options(nostack),
            );
        }
        SpinLockGuard {
            lock: self,
            saved_daif,
        }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    saved_daif: u64,
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.flag.store(false, Ordering::Release);
        interrupts::restore_irq(self.saved_daif);
    }
}

// ---------------------------------------------------------------------------
// IPIs
// ---------------------------------------------------------------------------

/// Kicks every other online CPU so it re-evaluates the run queues (a task may
/// have become Ready on this CPU). Implemented by `gic::send_sgi_all_but_self`
/// (SGI0); a no-op while only one CPU is online.
pub fn ipi_reschedule_others() {
    if cpu_count() > 1 {
        crate::gic::send_sgi_all_but_self(crate::gic::RESCHEDULE_IRQ);
    }
}

// ---------------------------------------------------------------------------
// Stop-the-world quiescence
// ---------------------------------------------------------------------------

/// Monotonic stop-the-world round counter. Each `quiesce_begin` takes a new
/// value; `0` means no round has ever started. A monotonically increasing
/// counter (instead of a boolean "active" flag) is what makes a round and its
/// acknowledgement unambiguous: a CPU parked in an older round can vouch for a
/// newer one instead of being woken *into* it by the same release store.
static QUIESCE_ROUND: AtomicU32 = AtomicU32::new(0);
/// Highest round that has been released. A parked CPU may leave once this
/// reaches the round it acknowledged, and a timing-out `quiesce_begin`
/// releases the world by storing its own round here.
static QUIESCE_RELEASE: AtomicU32 = AtomicU32::new(0);
/// Highest round each CPU has acknowledged. Monotonic (`fetch_max`), so a CPU
/// that never left the parked loop between two back-to-back rounds covers the
/// newer round without needing to re-enter the SGI handler. This closes the
/// straggler hang: with a plain boolean, the next round cleared the ACK and
/// reset RELEASE while that CPU was still spinning, so it could neither see
/// the new release nor take the new SGI (IRQs are masked inside the handler).
static QUIESCE_ACK: [AtomicU32; MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Opt-in `smp-selftest` observation: generation written by the boot CPU
/// before a round, and the value each parked CPU read back after its `dsb sy`.
#[cfg(feature = "smp-selftest")]
static SELFTEST_GENERATION: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "smp-selftest")]
static SELFTEST_SEEN: [AtomicU32; MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Parks the calling CPU until the quiescing CPU releases the world. Called
/// from the SGI1 handler and from `bkl_acquire` waiters (which cannot take an
/// IRQ because kernel code runs with IRQs masked). The caller must not hold
/// the BKL: the quiescing CPU owns it while the world is stopped.
///
/// The loop re-reads the round on every iteration. If a new round starts while
/// this CPU is still parked the CPU acknowledges the new round and keeps
/// waiting, so back-to-back rounds cannot strand it.
pub fn quiesce_ack_and_wait() {
    let cpu = this_cpu_id();
    loop {
        // Complete every memory access issued before the stop request before
        // telling the reaper that this CPU is quiescent. Re-issued per round
        // so a straggler's newer acknowledgement has the same ordering.
        unsafe {
            core::arch::asm!("dsb sy", options(nostack));
        }
        let round = QUIESCE_ROUND.load(Ordering::Acquire);
        if round == 0 {
            // No stop-the-world has ever started; nothing to park for.
            return;
        }
        #[cfg(feature = "smp-selftest")]
        if cpu < MAX_CPUS {
            SELFTEST_SEEN[cpu].store(
                SELFTEST_GENERATION.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
        if cpu < MAX_CPUS {
            QUIESCE_ACK[cpu].fetch_max(round, Ordering::Release);
        }
        while QUIESCE_RELEASE.load(Ordering::Acquire) < round {
            unsafe {
                core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
            }
        }
        // A newer round may have started while this CPU was parked. Keep
        // covering it until the round we acknowledged is the newest one.
        if QUIESCE_ROUND.load(Ordering::Acquire) == round {
            return;
        }
    }
}

/// Stops every other online CPU at a safe point. Must be called with the BKL
/// held; returns false (and releases everyone) on timeout, in which case the
/// caller must fail closed and leak. The caller must pair a `true` with
/// [`quiesce_end`].
pub fn quiesce_begin() -> bool {
    if cpu_count() <= 1 {
        return true;
    }
    // Start a new round. `QUIESCE_RELEASE` still names the previous round, so
    // `quiesce_active()` is true from this store until `quiesce_end` (or the
    // timeout branch) moves it forward. Only the BKL holder mutates the
    // counters, so the load/store pair needs no compare-exchange.
    let previous = QUIESCE_ROUND.load(Ordering::Acquire);
    let round = if previous == u32::MAX {
        // `0` is the "never started" sentinel. On wrap restart at 1 and clear
        // the acknowledgements, which name the old maximum round, so every CPU
        // has to vouch for the restarted sequence.
        for ack in &QUIESCE_ACK {
            ack.store(0, Ordering::Relaxed);
        }
        QUIESCE_RELEASE.store(0, Ordering::Release);
        1
    } else {
        previous + 1
    };
    QUIESCE_ROUND.store(round, Ordering::Release);
    crate::gic::send_sgi_all_but_self(crate::gic::QUIESCE_IRQ);

    let me = this_cpu_id();
    let mut spins: u32 = 0;
    loop {
        let mut all = true;
        for cpu in 0..cpu_count() {
            if cpu == me || !cpu_online(cpu) {
                continue;
            }
            if QUIESCE_ACK[cpu].load(Ordering::Acquire) < round {
                all = false;
                // Re-send periodically in case an SGI was coalesced while
                // the target was still in a masked kernel section.
                if spins % 100_000 == 0 && spins != 0 {
                    crate::gic::send_sgi_all_but_self(crate::gic::QUIESCE_IRQ);
                }
                break;
            }
        }
        if all {
            return true;
        }
        spins += 1;
        if spins >= 20_000_000 {
            // Fail closed: release the round so nobody stays parked, and let
            // the caller fall back to the conservative leak gate.
            QUIESCE_RELEASE.store(round, Ordering::Release);
            return false;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Releases CPUs parked by [`quiesce_begin`].
pub fn quiesce_end() {
    let round = QUIESCE_ROUND.load(Ordering::Acquire);
    QUIESCE_RELEASE.store(round, Ordering::Release);
}

/// True while a stop-the-world request is active. Derived from the round
/// counters so there is no separate flag to get out of sync with a timeout.
pub fn quiesce_active() -> bool {
    QUIESCE_RELEASE.load(Ordering::Acquire) < QUIESCE_ROUND.load(Ordering::Acquire)
}

/// Kernel-lock depth on this CPU (0 = not held). Used by kernel-task entry
/// code that must drop a lock inherited from its dispatcher.
pub fn bkl_depth() -> u32 {
    this_cpu().bkl_depth.load(Ordering::Relaxed)
}

/// True when an IRQ handler may run a scheduling pass. The interrupted
/// context must not already hold the kernel lock: a pass taken on top of a
/// task that is inside a syscall adds a second hold, and the block of the
/// next task consumes it, breaking the per-CPU depth accounting that carries
/// the lock across a context switch. Kernel code is therefore non-preemptible
/// under the BKL; it schedules at its own blocking points.
pub fn irq_may_schedule() -> bool {
    bkl_depth() == 0
}

/// Non-inline wrapper for asm callers (`silver_user_bootstrap` drops the
/// inherited kernel lock before `eret`-ing into user mode).
pub extern "C" fn bkl_release_asm() {
    bkl_release();
}

// ---------------------------------------------------------------------------
// AP bring-up
// ---------------------------------------------------------------------------

#[cfg(feature = "smp")]
use core::sync::atomic::Ordering as AtomicOrdering;

/// Boot parameters consumed by the physical AP trampoline. Fields are
/// atomics so the boot CPU can publish them with Release stores; the
/// trampoline reads them through its physical address after PSCI `CPU_ON`.
#[cfg(feature = "smp")]
#[repr(C)]
struct ApBootParams {
    ttbr0: AtomicUsize,
    ttbr1: AtomicUsize,
    mair: AtomicUsize,
    tcr: AtomicUsize,
    stack_top: AtomicUsize,
    entry: AtomicUsize,
}

#[cfg(feature = "smp")]
impl ApBootParams {
    const fn empty() -> Self {
        Self {
            ttbr0: AtomicUsize::new(0),
            ttbr1: AtomicUsize::new(0),
            mair: AtomicUsize::new(0),
            tcr: AtomicUsize::new(0),
            stack_top: AtomicUsize::new(0),
            entry: AtomicUsize::new(0),
        }
    }
}

/// One parameter block per CPU, so bringing up a second AP cannot overwrite
/// the block a first AP is still reading.
#[cfg(feature = "smp")]
static AP_PARAMS: [ApBootParams; MAX_CPUS] = [
    ApBootParams::empty(),
    ApBootParams::empty(),
    ApBootParams::empty(),
    ApBootParams::empty(),
];

// Physical AP trampoline. PSCI CPU_ON enters this at its physical address
// with the MMU off and x0 = context_id (the physical address of the
// `ApBootParams` block). Every instruction before the SCTLR write is
// position-independent and uses only x0-relative loads, so it executes at
// the load address even though the linked symbols are high-half addresses.
#[cfg(feature = "smp")]
core::arch::global_asm!(
    r#"
    .section .text.ap_entry, "ax"
    .global ap_entry
    .type ap_entry, %function
ap_entry:
    msr daifset, #0xf
    // AArch64 reset leaves FP/SIMD disabled. The Rust AP path contains
    // compiler-generated NEON (Vec zeroing/`swap_remove`), so enable it at
    // EL1 exactly like the boot CPU did.
    mov x1, #0x300000
    msr cpacr_el1, x1
    isb
    mov x19, x0                  // x0 = physical ApBootParams
    ldr x1, [x19, #0]            // TTBR0
    msr ttbr0_el1, x1
    ldr x1, [x19, #8]            // TTBR1
    msr ttbr1_el1, x1
    ldr x1, [x19, #16]           // MAIR_EL1
    msr mair_el1, x1
    ldr x1, [x19, #24]           // TCR_EL1
    msr tcr_el1, x1
    ldr x1, [x19, #32]           // stack top (high alias)
    mov sp, x1
    ldr x2, [x19, #40]           // high-half Rust entry
    isb
    mrs x1, sctlr_el1
    orr x1, x1, #1               // M
    orr x1, x1, #(1 << 22)       // EIS
    msr sctlr_el1, x1
    isb
    tlbi vmalle1
    dsb ish
    isb
    // MMU is on now; the literal pool and the entry are high-half addresses.
    ldr x1, =exception_vectors
    msr vbar_el1, x1
    isb
    br x2
    "#,
);

#[cfg(feature = "smp")]
extern "C" {
    fn ap_entry();
}

/// Rust body of the AP boot path. The trampoline above has already installed
/// TTBR0/TTBR1, enabled the MMU and switched SP to the high alias.
#[cfg(feature = "smp")]
extern "C" fn smp_ap_main() -> ! {
    // First Rust-side action: match the boot CPU's cache attributes before
    // touching shared kernel state (console, CPU slot, scheduler).
    let cache_state = crate::cache::enable_secondary();
    crate::console::write_str("SMP: AP MMU on\r\n");

    let Some(slot) = claim_cpu_slot() else {
        panic!("AP: no free per-CPU slot");
    };
    init_cpu_unpublished(slot);
    // Publish the read-back SCTLR before this CPU goes online, so the boot
    // self-test never sees an online AP without a recorded cache state.
    crate::cache::record_secondary(slot, cache_state);
    crate::console::write_str("SMP: AP slot claimed\r\n");
    if !crate::gic::cpu_init() {
        // No redistributor/CPU interface means no banked SGI0/SGI1. Publishing
        // this CPU would make quiescence wait forever for an ack it can never
        // receive, so park it offline instead.
        crate::console::write_str("SMP: AP GIC unavailable, CPU left offline\r\n");
        loop {
            unsafe {
                core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
            }
        }
    }
    crate::console::write_str("SMP: AP gic ok\r\n");
    crate::timer::cpu_init();
    crate::console::write_str("SMP: AP timer ok\r\n");
    publish_cpu(slot);

    crate::console::write_str("SMP: CPU");
    crate::console::write_dec_u64(slot as u64);
    crate::console::write_str(" online, mpidr=0x");
    crate::console::write_hex_u64(this_cpu().mpidr);
    crate::console::write_str("\r\n");

    // Opt-in cross-PE TLB proof: park here (IRQs still masked, no BKL held)
    // until the boot CPU publishes a scratch address space. Running the probe
    // before the scheduler loop means the boot CPU can drive the peer without
    // the BKL deadlock a reschedule IPI would risk.
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    ap_tlb_probe_wait();

    // Same contract as the boot CPU: kernel tasks run with IRQs enabled and
    // pick up work through the scheduler. Publish to the stall watchdog first:
    // a PE parked in `ap_tlb_probe_wait` must not be judged as hung.
    crate::watchdog::cpu_started(this_cpu_id());
    interrupts::enable_irq();
    // Take at least one timer PPI before entering the scheduler loop. The
    // heartbeat selftest uses `cpu_ticks > 0` as proof that this PE's timer is
    // wired, but a PE blocked in `bkl_acquire` runs with IRQs masked and
    // cannot take another tick; without this window the test is a race.
    let cpu = this_cpu_id();
    let tick_deadline = crate::timer::ticks().saturating_add(50);
    while crate::timer::cpu_ticks(cpu) == 0 && crate::timer::ticks() < tick_deadline {
        core::hint::spin_loop();
    }
    loop {
        crate::scheduler::schedule();
        interrupts::wait_for_irq();
    }
}

/// Powers the secondary CPUs on via PSCI `CPU_ON` (QEMU virt conduit is
/// `hvc`; the tablet profile stays single-core and never reaches here).
/// Called once, from the first scheduling pass, with the kernel lock held —
/// after the boot CPU has already entered its first task, so no AP can race
/// the boot-time context switch.
#[cfg(feature = "smp")]
pub fn boot_secondary_cpus() {
    if cpu_count() <= 1 {
        return;
    }
    crate::console::write_str("SMP: powering on secondary CPUs\r\n");
    EXPECTED_CPUS.store(1, Ordering::Release);
    for cpu in 1..cpu_count() {
        let Some((stack, stack_generation)) =
            crate::frame_allocator::alloc_contiguous_pages_tagged(2)
        else {
            crate::console::write_str("SMP: no stack for CPU");
            crate::console::write_dec_u64(cpu as u64);
            crate::console::write_str(", leaving it offline\r\n");
            return;
        };
        let Some((private, private_generation)) = crate::frame_allocator::alloc_page_tagged()
        else {
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: no private page for CPU");
            crate::console::write_dec_u64(cpu as u64);
            crate::console::write_str(", leaving it offline\r\n");
            return;
        };
        let Some((ttbr0, space_token)) = crate::mmu::create_task_space(private, 0, None) else {
            crate::frame_allocator::free_page_tagged(private, private_generation);
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: no task space for CPU");
            crate::console::write_dec_u64(cpu as u64);
            crate::console::write_str(", leaving it offline\r\n");
            return;
        };
        if !crate::mmu::install_ap_identity(ttbr0) {
            // No AP was started, so the freshly built space is not running
            // anywhere and can be torn down without the quiescence gate.
            crate::mmu::destroy_task_space(
                space_token,
                private,
                private_generation,
                crate::mmu::asid_of(ttbr0),
            );
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: no AP identity map for CPU");
            crate::console::write_dec_u64(cpu as u64);
            crate::console::write_str(", leaving it offline\r\n");
            return;
        }

        // The AP's bootstrap stack must not inherit another owner's bytes.
        unsafe {
            core::ptr::write_bytes(
                crate::memory::phys_to_virt(stack) as *mut u8,
                0,
                2 * crate::memory::PAGE_SIZE,
            );
        }
        // The AP runs with caches off until its first Rust instruction and
        // writes this stack through non-cacheable normal memory. Clean the
        // zeroing writes now: a dirty line left in this CPU's cache would be
        // written back later and clobber the AP's live stack.
        crate::mmu::clean_range_to_poc(
            crate::memory::phys_to_virt(stack),
            2 * crate::memory::PAGE_SIZE,
        );

        // The AP starts with cold caches. Publish its parameter block and
        // page-table root through the high alias (the boot CPU has no low
        // identity map left), then pass the block's physical address as the
        // PSCI context id. `_space_token` is deliberately kept alive by the
        // registry: an AP idle space is never retired in Phase 1.
        let params = &AP_PARAMS[cpu];
        params.ttbr0.store(ttbr0, AtomicOrdering::Release);
        params
            .ttbr1
            .store(crate::mmu::kernel_ttbr1(), AtomicOrdering::Release);
        params
            .mair
            .store(crate::mmu::ap_boot_mair() as usize, AtomicOrdering::Release);
        params
            .tcr
            .store(crate::mmu::ap_boot_tcr() as usize, AtomicOrdering::Release);
        params.stack_top.store(
            crate::memory::phys_to_virt(stack + 2 * crate::memory::PAGE_SIZE),
            AtomicOrdering::Release,
        );
        params
            .entry
            .store(smp_ap_main as *const () as usize, AtomicOrdering::Release);

        let params_va = params as *const ApBootParams as usize;
        let params_pa = crate::memory::virt_to_phys(params_va);
        crate::mmu::clean_range_to_poc(params_va, core::mem::size_of::<ApBootParams>());
        crate::mmu::clean_range_to_poc(
            crate::memory::phys_to_virt(crate::mmu::root_of(ttbr0)),
            crate::memory::PAGE_SIZE,
        );

        // QEMU virt numbers CPUs by Aff0: cpu index == affinity value.
        let target_mpidr = cpu as u64;
        let entry_physical = crate::memory::virt_to_phys(ap_entry as *const () as usize);
        let result = psci_cpu_on(target_mpidr, entry_physical, params_pa);
        crate::console::write_str("SMP: CPU_ON cpu=");
        crate::console::write_dec_u64(cpu as u64);
        crate::console::write_str(" -> ");
        crate::console::write_dec_u64(result as u64);
        crate::console::write_str("\r\n");
        if result != 0 {
            // The CPU never entered the trampoline, so nothing references
            // this stack or address space. Release both instead of leaking
            // them, and keep probing the remaining slots: a QEMU machine with
            // fewer CPUs than `MAX_CPUS` simply reports every missing slot as
            // offline instead of aborting the whole bring-up.
            crate::mmu::destroy_task_space(
                space_token,
                private,
                private_generation,
                crate::mmu::asid_of(ttbr0),
            );
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: CPU_ON failed, CPU left offline\r\n");
            continue;
        }
        // PSCI accepted this CPU; the self-test will wait for it to publish.
        EXPECTED_CPUS.fetch_add(1, Ordering::Release);
    }

    #[cfg(feature = "smp-selftest")]
    {
        // The peers are still parked in `ap_tlb_probe_wait` with no current
        // task, which is exactly the window the remap sentinel test needs.
        // Wait for them to publish `online` first (they park right after it).
        let _ = wait_for_online_peers();
        // Pure decision policy, linked into the kernel.
        crate::watchdog::run_policy_selftest();
        crate::scheduler::run_remap_sentinel_selftest();
        // Proves the ready queues are FIFO: a stack-shaped queue would starve
        // the oldest of three runnable tasks forever.
        crate::scheduler::run_ready_queue_selftest();
        // Priority classes with aging: high first, FIFO inside a class, and
        // a low-class probe promoted under constant high-class pressure.
        crate::scheduler::run_priority_selftest();
        // The peers are parked in `ap_tlb_probe_wait` right now, so the
        // cross-PE TLB proof must run before the stop-the-world rounds.
        run_cross_pe_tlb_probe();
        // The APs are in their scheduler loops once the TLB probe completes,
        // so every watched CPU must be taking its timer PPI now.
        crate::watchdog::run_heartbeat_selftest();
        // Every online CPU must have enabled C/I in its own SCTLR. This runs
        // before the stop-the-world and heavy stress tests so a missing cache
        // enable is reported as a plain boot failure.
        crate::cache::run_cache_selftest();
        // The APs are in their scheduler loops trying to take the lock this
        // CPU holds, which is exactly the waiter state the detector watches.
        run_bkl_stall_selftest();
        run_quiesce_selftest();
        run_clock_selftest();
        // Last: spawn short-lived tasks and let the APs run/reap them while
        // the boot CPU waits with the BKL released. Opt-in `smp-churn`: it
        // currently exposes an intermittent scheduler-index race, so it must
        // not run in the normal selftest profile.
        #[cfg(feature = "smp-churn")]
        {
            crate::scheduler::run_smp_churn_selftest();
            // By now the APs have completed passes and sit on their idle
            // tasks, so a real idle peer exists for the push probe.
            crate::scheduler::run_push_selftest();
            // Negative test: a task that really spins with IRQs masked must be
            // reported by the monitor on another PE.
            crate::scheduler::run_stall_selftest();
        }
    }
}

// ---------------------------------------------------------------------------
// Global clock self-test
// ---------------------------------------------------------------------------

/// Proves that global time does not depend on the boot CPU taking its timer
/// PPI. The boot CPU masks IRQs and waits for one tick; with the old
/// boot-CPU-only counter this would never advance and every `sleep` deadline
/// would freeze for as long as CPU0 stays masked. The system counter is
/// independent of both the PPI and the CPU, so one tick is reached regardless.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn run_clock_selftest() {
    let start = crate::timer::ticks();
    let target = start.saturating_add(1);
    let saved_daif = crate::interrupts::save_and_disable_irq();
    let mut spins: u32 = 0;
    while crate::timer::ticks() < target && spins < 20_000_000 {
        spins += 1;
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    let end = crate::timer::ticks();
    crate::interrupts::restore_irq(saved_daif);
    if end >= target {
        crate::console::write_str("SMP SELFTEST: clock PASS (CPU0 masked, ticks +");
        crate::console::write_dec_u64(end - start);
        crate::console::write_str(")\r\n");
    } else {
        crate::console::write_str("SMP SELFTEST: clock FAIL (start=");
        crate::console::write_dec_u64(start);
        crate::console::write_str(" end=");
        crate::console::write_dec_u64(end);
        crate::console::write_str(" spins=");
        crate::console::write_dec_u64(spins as u64);
        crate::console::write_str(")\r\n");
    }
}

// ---------------------------------------------------------------------------
// Cross-PE TLB shootdown self-test
// ---------------------------------------------------------------------------

/// Mailbox phases for the opt-in cross-PE TLB proof. `0` means "no test yet";
/// peers spin on [`TLB_PROBE_ABORT`] or a real phase value.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const TLB_PROBE_IDLE: u32 = 0;
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const TLB_PROBE_READ_A: u32 = 1;
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const TLB_PROBE_READ_B: u32 = 2;
/// Releases a peer parked in [`ap_tlb_probe_wait`] when the boot CPU could not
/// run the probe. Must be `>= TLB_PROBE_READ_B` so both spin loops exit.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const TLB_PROBE_ABORT: u32 = 0xFF;
/// Bound on every probe wait. A peer that never arrives is reported, not hung.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const TLB_PROBE_TIMEOUT_SPINS: u32 = 50_000_000;

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static TLB_PROBE_PHASE: AtomicU32 = AtomicU32::new(TLB_PROBE_IDLE);
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static TLB_PROBE_TTBR0: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static TLB_PROBE_VA: AtomicUsize = AtomicUsize::new(0);
/// Last value each peer read through the scratch VA (phase A, then phase B).
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static TLB_PROBE_SEEN: [AtomicU64; MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static TLB_PROBE_ACK: [AtomicU32; MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Waits until every PSCI-accepted CPU is published, then returns the number of
/// online peers. `None` means no secondary came up at all. Mirrors the wait the
/// quiesce self-test used to inline.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn wait_for_online_peers() -> Option<usize> {
    let me = this_cpu_id();
    let expected = expected_cpus();
    let mut spins: u32 = 0;
    loop {
        let online = (0..cpu_count())
            .filter(|&cpu| cpu != me && cpu_online(cpu))
            .count();
        if online + 1 >= expected {
            return Some(online);
        }
        spins += 1;
        if spins >= 5_000_000 {
            if online > 0 {
                // PSCI accepted more CPUs than published: a GIC failure parks
                // an AP offline by design. Test the set that is actually up.
                crate::console::write_str("SMP SELFTEST: waiting for all CPUs gave up\r\n");
                return Some(online);
            }
            crate::console::write_str("SMP SELFTEST: skipped, no secondary online\r\n");
            return None;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
}

/// AP half of the cross-PE TLB proof. Runs on the AP with IRQs masked and no
/// kernel lock, before the scheduler loop: switch to the boot CPU's scratch
/// address space, read the probe VA, park until the boot CPU has re-pointed
/// the PTE, read the *same* VA again, then restore the original TTBR0.
///
/// The second read is the whole point: it can only observe the new frame if
/// the boot CPU's `tlbi vmalle1is` really invalidated this PE's stale entry.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn ap_tlb_probe_wait() {
    let mut spins: u32 = 0;
    loop {
        let phase = TLB_PROBE_PHASE.load(Ordering::Acquire);
        if phase != TLB_PROBE_IDLE {
            break;
        }
        spins += 1;
        if spins >= TLB_PROBE_TIMEOUT_SPINS {
            // No probe was published (single-CPU run or boot gave up).
            return;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    if TLB_PROBE_PHASE.load(Ordering::Acquire) == TLB_PROBE_ABORT {
        return;
    }
    let cpu = this_cpu_id();
    if cpu >= MAX_CPUS {
        return;
    }
    let ttbr0 = TLB_PROBE_TTBR0.load(Ordering::Acquire);
    let va = TLB_PROBE_VA.load(Ordering::Acquire);
    let saved: usize;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) saved, options(nostack));
        // ASID 0 is used by every space, so a switch must drop this PE's whole
        // EL1&0 TLB. `vmalle1` (local) is enough here; the peer only needs its
        // own stale entries gone before the first read.
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) ttbr0 as u64,
            options(nostack),
        );
    }
    let first = unsafe { core::ptr::read_volatile(va as *const u64) };
    TLB_PROBE_SEEN[cpu].store(first, Ordering::Release);
    TLB_PROBE_ACK[cpu].store(TLB_PROBE_READ_A, Ordering::Release);

    while TLB_PROBE_PHASE.load(Ordering::Acquire) < TLB_PROBE_READ_B {
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    let second = unsafe { core::ptr::read_volatile(va as *const u64) };
    TLB_PROBE_SEEN[cpu].store(second, Ordering::Release);

    // Restore before acknowledging: the boot CPU retires the scratch space as
    // soon as every peer reports phase B.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) saved as u64,
            options(nostack),
        );
    }
    TLB_PROBE_ACK[cpu].store(TLB_PROBE_READ_B, Ordering::Release);
}

/// A scratch address space plus the two probe frames, torn down on every exit
/// path (including the early-failure returns) by `Drop`.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
struct TlbProbeScratch {
    ttbr0: usize,
    token: silver_microkernel::table_registry::TableToken,
    private: usize,
    private_generation: u32,
    frames: [(usize, u32); 2],
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
impl TlbProbeScratch {
    fn new() -> Option<Self> {
        let (private, private_generation) = crate::frame_allocator::alloc_page_tagged()?;
        let Some((ttbr0, token)) = crate::mmu::create_task_space(private, 0, None) else {
            crate::frame_allocator::free_page_tagged(private, private_generation);
            return None;
        };
        let mut frames = [(0usize, 0u32); 2];
        for slot in &mut frames {
            let Some((frame, generation)) = crate::frame_allocator::alloc_page_tagged() else {
                crate::mmu::destroy_task_space(
                    token,
                    private,
                    private_generation,
                    crate::mmu::asid_of(ttbr0),
                );
                for &(frame, generation) in frames.iter() {
                    // Generation 0 means "slot never filled".
                    if generation != 0 {
                        crate::frame_allocator::free_page_tagged(frame, generation);
                    }
                }
                return None;
            };
            *slot = (frame, generation);
        }
        Some(Self {
            ttbr0,
            token,
            private,
            private_generation,
            frames,
        })
    }
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
impl Drop for TlbProbeScratch {
    fn drop(&mut self) {
        // Every peer acknowledged phase B only after restoring its own TTBR0,
        // so no online CPU can still be walking these tables.
        crate::mmu::destroy_task_space(
            self.token,
            self.private,
            self.private_generation,
            crate::mmu::asid_of(self.ttbr0),
        );
        for &(frame, generation) in self.frames.iter() {
            crate::frame_allocator::free_page_tagged(frame, generation);
        }
    }
}

/// Blocks until every online peer acknowledges `phase` (`>= peers` of them).
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn wait_for_probe_acks(phase: u32, peers: usize) -> bool {
    let me = this_cpu_id();
    let mut spins: u32 = 0;
    loop {
        let mut online = 0usize;
        let mut missing = 0usize;
        for cpu in 0..cpu_count() {
            if cpu == me || !cpu_online(cpu) {
                continue;
            }
            online += 1;
            if TLB_PROBE_ACK[cpu].load(Ordering::Acquire) < phase {
                missing += 1;
            }
        }
        if online >= peers && missing == 0 {
            return true;
        }
        spins += 1;
        if spins >= TLB_PROBE_TIMEOUT_SPINS {
            return false;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Boot-CPU half: publish a scratch mapping, let every peer read it, re-point
/// the same VA at another frame with a batched broadcast invalidation, and
/// require every peer to observe the new frame.
///
/// This is the cross-PE counterpart to [`crate::mmu::run_tlb_self_test`]: the
/// boot test catches a missing `tlbi` on the *executing* PE, this one catches a
/// flush that only invalidates locally. Runs with the kernel lock held; the
/// peers are parked inside [`ap_tlb_probe_wait`] and never take the BKL, so
/// there is no lock-ordering deadlock.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn run_cross_pe_tlb_probe() {
    if cpu_count() <= 1 {
        return;
    }
    let Some(peers) = wait_for_online_peers() else {
        return;
    };
    if peers == 0 {
        TLB_PROBE_PHASE.store(TLB_PROBE_ABORT, Ordering::Release);
        return;
    }

    for cpu in 0..MAX_CPUS {
        TLB_PROBE_SEEN[cpu].store(0, Ordering::Relaxed);
        TLB_PROBE_ACK[cpu].store(0, Ordering::Relaxed);
    }
    let Some(scratch) = TlbProbeScratch::new() else {
        TLB_PROBE_PHASE.store(TLB_PROBE_ABORT, Ordering::Release);
        crate::console::write_str("SMP SELFTEST: tlb cross-PE FAIL (no scratch space)\r\n");
        return;
    };

    const VALUE_A: u64 = 0xA5A5_5A5A_C3C3_3C3C;
    const VALUE_B: u64 = 0x5A5A_A5A5_3C3C_C3C3;
    let va = crate::mmu::USER_MMAP_VA;
    let (frame_a, _) = scratch.frames[0];
    let (frame_b, _) = scratch.frames[1];

    if !crate::mmu::map_user_pages_at(scratch.ttbr0, va, frame_a, 1, crate::mmu::PROT_WRITE) {
        TLB_PROBE_PHASE.store(TLB_PROBE_ABORT, Ordering::Release);
        crate::console::write_str("SMP SELFTEST: tlb cross-PE FAIL (map)\r\n");
        return;
    }
    // Values are written after publication. Both the kernel alias and the user
    // alias are Normal inner-shareable, so a peer's read snoops this store; the
    // Release store of the phase orders it before the peer's first load.
    unsafe {
        core::ptr::write_volatile(crate::memory::phys_to_virt(frame_a) as *mut u64, VALUE_A);
        core::ptr::write_volatile(crate::memory::phys_to_virt(frame_b) as *mut u64, VALUE_B);
    }

    TLB_PROBE_TTBR0.store(scratch.ttbr0, Ordering::Release);
    TLB_PROBE_VA.store(va, Ordering::Release);
    TLB_PROBE_PHASE.store(TLB_PROBE_READ_A, Ordering::Release);

    let first_ok = wait_for_probe_acks(TLB_PROBE_READ_A, peers);
    let remapped = first_ok
        && crate::mmu::replace_user_page(scratch.ttbr0, va, frame_b, crate::mmu::PROT_WRITE);
    if remapped {
        // `replace_user_page` returns only after its `dsb ish` completed the
        // broadcast, so a peer that now reads VA must re-walk the table.
        TLB_PROBE_PHASE.store(TLB_PROBE_READ_B, Ordering::Release);
    }
    let second_ok = remapped && wait_for_probe_acks(TLB_PROBE_READ_B, peers);
    if !second_ok {
        // Release any peer still parked; a later peer exits immediately too.
        TLB_PROBE_PHASE.store(TLB_PROBE_ABORT, Ordering::Release);
        crate::console::write_str("SMP SELFTEST: tlb cross-PE FAIL (timeout)\r\n");
        return;
    }

    let me = this_cpu_id();
    let mut stale = 0u64;
    for cpu in 0..cpu_count() {
        if cpu == me || !cpu_online(cpu) {
            continue;
        }
        if TLB_PROBE_SEEN[cpu].load(Ordering::Acquire) != VALUE_B {
            stale += 1;
        }
    }
    // Every peer already left the probe; leave ABORT set so a CPU that comes
    // online late never parks in the idle loop.
    TLB_PROBE_PHASE.store(TLB_PROBE_ABORT, Ordering::Release);
    if stale != 0 {
        crate::console::write_str("SMP SELFTEST: tlb cross-PE FAIL (");
        crate::console::write_dec_u64(stale);
        crate::console::write_str(" CPU(s) kept the stale translation)\r\n");
        return;
    }
    crate::console::write_str("SMP SELFTEST: tlb cross-PE PASS (");
    crate::console::write_dec_u64(peers as u64);
    crate::console::write_str(" peer CPU(s) saw the remap)\r\n");
}

/// Number of back-to-back stop-the-world rounds in the opt-in self-test.
/// Consecutive rounds (no yield in between) exercise the straggler path: a CPU
/// still parked from round N must acknowledge round N+1 instead of being
/// stranded by a cleared flag.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
const SELFTEST_ROUNDS: u32 = 64;

/// Opt-in boot self-test: wait for a published secondary, then run
/// [`SELFTEST_ROUNDS`] real SGI1 stop-the-world rounds. Each round also proves
/// memory visibility: the boot CPU publishes a generation before the request,
/// every acking CPU reads it after its own `dsb sy`, and the boot CPU checks
/// the observation while the world is still stopped.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn run_quiesce_selftest() {
    if cpu_count() <= 1 {
        return;
    }
    let Some(peers) = wait_for_online_peers() else {
        return;
    };

    let me = this_cpu_id();

    // The peer may have published but not yet unmasked IRQs. `quiesce_begin`
    // re-sends SGI1 and the BKL retry loop acknowledges once IRQs are enabled.
    for round in 1..=SELFTEST_ROUNDS {
        SELFTEST_GENERATION.store(round, Ordering::Release);
        unsafe {
            core::arch::asm!("dsb sy", options(nostack));
        }
        if !quiesce_begin() {
            crate::console::write_str("SMP SELFTEST: quiesce FAIL (timeout, round ");
            crate::console::write_dec_u64(round as u64);
            crate::console::write_str(")\r\n");
            return;
        }
        // The world is stopped, so no peer can change its observation: every
        // CPU that acknowledged this round must have seen the new generation.
        let mut stale = 0u64;
        for cpu in 0..cpu_count() {
            if cpu == me || !cpu_online(cpu) {
                continue;
            }
            if SELFTEST_SEEN[cpu].load(Ordering::Acquire) != round {
                stale += 1;
            }
        }
        quiesce_end();
        if stale != 0 {
            crate::console::write_str("SMP SELFTEST: quiesce FAIL (");
            crate::console::write_dec_u64(stale);
            crate::console::write_str(" CPU(s) missed generation at round ");
            crate::console::write_dec_u64(round as u64);
            crate::console::write_str(")\r\n");
            return;
        }
    }

    crate::console::write_str("SMP SELFTEST: quiesce PASS (");
    crate::console::write_dec_u64(SELFTEST_ROUNDS as u64);
    crate::console::write_str(" rounds, ");
    crate::console::write_dec_u64(peers as u64);
    crate::console::write_str(" peer CPU(s))\r\n");

    // Timer liveness: the stop-the-world handshake proves a peer can take an
    // SGI from the BKL retry loop, which does not require IRQs. Check each
    // published CPU's own PPI counter as well, so an online-but-deaf core is
    // reported instead of looking healthy until a deadline stops expiring.
    //
    // The observation window must not hold the BKL. Once real tasks are
    // running (the demo+selftest profile), an AP that entered `schedule` from
    // a timer IRQ spins in `bkl_acquire` with IRQs masked; if this CPU kept
    // the lock, that peer could never take another PPI and a healthy system
    // would report a false failure.
    bkl_release();
    let ticks_ok = {
        let mut baseline = [0_u64; MAX_CPUS];
        for (cpu, value) in baseline.iter_mut().enumerate() {
            *value = crate::timer::cpu_ticks(cpu);
        }
        let mut spins: u32 = 0;
        loop {
            let mut all = true;
            for cpu in 0..cpu_count() {
                if cpu == me || !cpu_online(cpu) {
                    continue;
                }
                if crate::timer::cpu_ticks(cpu) <= baseline[cpu] {
                    all = false;
                }
            }
            if all {
                break true;
            }
            spins += 1;
            if spins >= 50_000_000 {
                break false;
            }
            unsafe {
                core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
            }
        }
    };
    bkl_acquire();
    if !ticks_ok {
        crate::console::write_str("SMP SELFTEST: ticks FAIL (a CPU missed its timer PPI)\r\n");
        return;
    }
    crate::console::write_str("SMP SELFTEST: ticks PASS (");
    crate::console::write_dec_u64(peers as u64);
    crate::console::write_str(" peer CPU(s))\r\n");
}

/// PSCI CPU_ON (SMC64 function id 0xC4000003) over QEMU `virt`'s HVC
/// conduit. Returns the PSCI status: 0 = SUCCESS. `context_id` is delivered
/// to the entry point in x0.
#[cfg(feature = "smp")]
fn psci_cpu_on(target_mpidr: u64, entry_physical: usize, context_id: usize) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inlateout("x0") 0xC400_0003_u64 => ret,
            in("x1") target_mpidr,
            in("x2") entry_physical as u64,
            in("x3") context_id as u64,
            options(nostack),
        );
    }
    ret
}
