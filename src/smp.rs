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
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};

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
    /// Kernel-lock nesting depth on this CPU (0 = not held here).
    bkl_depth: UnsafeCell<u32>,
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
            bkl_depth: UnsafeCell::new(0),
        }
    }
}

static CPUS: [PerCpu; MAX_CPUS] = [
    PerCpu::empty(),
    PerCpu::empty(),
    PerCpu::empty(),
    PerCpu::empty(),
];

/// Number of CPU slots the platform may bring up. QEMU `virt` uses up to
/// [`MAX_CPUS`] (the profile is tested with `-smp 4`); CPUs the machine does
/// not expose simply fail `PSCI CPU_ON` and stay offline. Both real-hardware
/// profiles stay single-core: their boot stub parks secondaries and AP
/// bring-up is QEMU-only.
pub fn cpu_count() -> usize {
    #[cfg(feature = "smp")]
    {
        if platform::detect().is_tablet() || platform::detect().is_fog() {
            1
        } else {
            MAX_CPUS
        }
    }
    #[cfg(not(feature = "smp"))]
    {
        let _ = platform::detect();
        1
    }
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
#[inline]
pub fn set_current_index(index: usize) {
    unsafe {
        *this_cpu().current_index.get() = index;
    }
}

/// Reads another (online) CPU's current index — used by reap to reposition
/// every CPU after the task vector shrinks. Kernel lock must be held.
#[inline]
pub fn current_index_of(cpu: usize) -> usize {
    unsafe { *CPUS[cpu].current_index.get() }
}

/// Repositions another CPU's current index. Kernel lock must be held.
#[inline]
pub fn set_current_index_for(cpu: usize, index: usize) {
    unsafe {
        *CPUS[cpu].current_index.get() = index;
    }
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

/// Enables AP task execution once the per-CPU idle contexts exist. Kept as a
/// runtime switch for bisect/rollback; `smp` builds default to true.
#[cfg(feature = "smp")]
pub static AP_TASK_SCHEDULING: AtomicBool = AtomicBool::new(true);

/// CPUs PSCI accepted (`CPU_ON` returned success) plus the boot CPU. The
/// self-test waits for exactly this many CPUs, so a `-smp 2` machine does not
/// block on slots the firmware rejected while `-smp 4` waits for all three APs.
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
    if crate::smp::BKL_DISABLED.load(Ordering::Relaxed) { return; }
    let cpu = this_cpu();
    let depth = unsafe { &mut *cpu.bkl_depth.get() };
    if *depth > 0 {
        *depth += 1;
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
    let mut spins: u32 = 0;
    loop {
        if quiesce_active() && KERNEL_LOCK.owner.load(Ordering::Relaxed) != cpu.cpu_id as isize {
            quiesce_ack_and_wait();
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
        spins += 1;
        if spins == 50_000_000 {
            crate::console::write_str("BKL STUCK: owner=");
            crate::console::write_hex_u64(KERNEL_LOCK.owner.load(Ordering::Relaxed) as u64);
            crate::console::write_str(" depth=");
            crate::console::write_dec_u64(*depth as u64);
            crate::console::write_str("\r\n");
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    *depth = 1;
}

/// Releases the kernel lock. Must be paired with [`bkl_acquire`].
#[inline]
pub fn bkl_release() {
    if crate::smp::BKL_DISABLED.load(Ordering::Relaxed) { return; }
    let cpu = this_cpu();
    let depth = unsafe { &mut *cpu.bkl_depth.get() };
    debug_assert!(*depth > 0, "bkl_release without acquire");
    *depth -= 1;
    if *depth == 0 {
        KERNEL_LOCK.owner.store(BKL_FREE, Ordering::Release);
    }
}

/// Debug aid: documents that a mutation site relies on the caller holding the
/// kernel lock (no-op outside debug builds).
#[inline]
pub fn bkl_assert_held() {
    debug_assert!(unsafe { *this_cpu().bkl_depth.get() } > 0, "kernel lock not held");
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
    unsafe { *this_cpu().bkl_depth.get() }
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
    crate::console::write_str("SMP: AP MMU on\r\n");

    let Some(slot) = claim_cpu_slot() else {
        panic!("AP: no free per-CPU slot");
    };
    init_cpu_unpublished(slot);
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

    // Same contract as the boot CPU: kernel tasks run with IRQs enabled and
    // pick up work through the scheduler.
    interrupts::enable_irq();
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
            crate::mmu::destroy_task_space(space_token, private, private_generation);
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
            crate::memory::phys_to_virt(ttbr0),
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
            crate::mmu::destroy_task_space(space_token, private, private_generation);
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: CPU_ON failed, CPU left offline\r\n");
            continue;
        }
        // PSCI accepted this CPU; the self-test will wait for it to publish.
        EXPECTED_CPUS.fetch_add(1, Ordering::Release);
    }

    #[cfg(feature = "smp-selftest")]
    run_quiesce_selftest();
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
    let me = this_cpu_id();
    let expected = expected_cpus();
    let mut spins: u32 = 0;
    let peers = loop {
        let online = (0..cpu_count())
            .filter(|&cpu| cpu != me && cpu_online(cpu))
            .count();
        if online + 1 >= expected {
            break online;
        }
        spins += 1;
        if spins >= 5_000_000 {
            if online > 0 {
                // PSCI accepted more CPUs than published: a GIC failure parks
                // an AP offline by design. Test the set that is actually up.
                crate::console::write_str("SMP SELFTEST: waiting for all CPUs gave up\r\n");
                break online;
            }
            crate::console::write_str("SMP SELFTEST: skipped, no secondary online\r\n");
            return;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    };

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
            break;
        }
        spins += 1;
        if spins >= 50_000_000 {
            crate::console::write_str("SMP SELFTEST: ticks FAIL (a CPU missed its timer PPI)\r\n");
            return;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
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
