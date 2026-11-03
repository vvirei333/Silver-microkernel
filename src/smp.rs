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
use core::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

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

/// Number of CPUs the platform brings up. The tablet profile stays
/// single-core for now (its ABL already parks secondaries in the boot stub);
/// QEMU runs with `-smp 2`.
pub fn cpu_count() -> usize {
    // Both real-hardware profiles stay single-core for now: the boot stub
    // parks secondaries and AP bring-up is not implemented. QEMU runs 2.
    if platform::detect().is_tablet() || platform::detect().is_fog() {
        1
    } else {
        2
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
    let cpu = &CPUS[slot];
    // Writes race with nothing: the slot is not yet published (`online` is
    // set below with Release), and no other CPU reads this slot's fields.
    unsafe {
        let cpu = cpu as *const PerCpu as *mut PerCpu;
        (*cpu).cpu_id = slot;
        (*cpu).mpidr = read_mpidr();
    }
    unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) cpu as *const PerCpu as usize, options(nostack));
    }
    cpu.online.store(true, Ordering::Release);
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
        if !cpu.online.swap(true, Ordering::AcqRel) {
            // Slot claimed exclusively (the swap above is the handoff); no
            // other CPU touches its fields until it publishes its state.
            unsafe {
                let cpu = cpu as *const PerCpu as *mut PerCpu;
                (*cpu).cpu_id = slot;
                (*cpu).mpidr = read_mpidr();
            }
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
    // The retry loop must be opaque to the optimizer: an empty-body
    // `while cmpxchg { spin_loop() }` was lowered by LLVM to a bare `b .`
    // (the atomic retry got deleted with the loop), hanging the kernel on
    // the first contended acquire. The asm `yield` makes the loop real.
    let mut spins: u32 = 0;
    loop {
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
    // BISECT: disabled
    if false {
        crate::gic::send_sgi_all_but_self(crate::gic::RESCHEDULE_IRQ);
    }
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

use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// Boot parameters handed to the next AP. Published (and cleaned to the
/// point of coherency) by the boot CPU before PSCI CPU_ON.
static AP_TTBR0: AtomicUsize = AtomicUsize::new(0);

// Entry stub for APs: PSCI CPU_ON starts the CPU here with the MMU off,
// x0 = kernel stack pointer (a physical address; the kernel's low half is
// identity-mapped, so it is valid with and without the MMU).
core::arch::global_asm!(
    r#"
    .section .text.ap_entry, "ax"
    .global ap_entry
    .type ap_entry, %function
ap_entry:
    msr daifset, #0xf
    mov sp, x0
    // VBAR first: any exception taken before this would vector to 0x0.
    ldr x1, =exception_vectors
    msr vbar_el1, x1
    bl {ap_main}
    "#,
    ap_main = sym smp_ap_main
);

extern "C" {
    fn ap_entry();
}

/// Rust body of the AP boot path. Runs with the MMU off until `ap_enable`.
extern "C" fn smp_ap_main() -> ! {
    let ttbr0 = AP_TTBR0.load(AtomicOrdering::Acquire);
    crate::mmu::ap_enable(ttbr0);
    crate::console::write_str("SMP: AP MMU on\r\n");

    let Some(slot) = claim_cpu_slot() else {
        panic!("AP: no free per-CPU slot");
    };
    init_cpu(slot);
    crate::console::write_str("SMP: AP slot claimed\r\n");
    crate::gic::cpu_init();
    crate::console::write_str("SMP: AP gic ok\r\n");
    crate::timer::cpu_init();
    crate::console::write_str("SMP: AP timer ok\r\n");

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
pub fn boot_secondary_cpus() {
    if cpu_count() <= 1 {
        return;
    }
    crate::console::write_str("SMP: powering on secondary CPUs\r\n");
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
        let Some((ttbr0, _space_token)) = crate::mmu::create_task_space(private, 0, None) else {
            crate::frame_allocator::free_page_tagged(private, private_generation);
            crate::frame_allocator::free_contiguous_pages_tagged(stack, 2, stack_generation);
            crate::console::write_str("SMP: no task space for CPU");
            crate::console::write_dec_u64(cpu as u64);
            crate::console::write_str(", leaving it offline\r\n");
            return;
        };

        AP_TTBR0.store(ttbr0, AtomicOrdering::Release);
        // The AP starts with cold caches: push the boot parameter and the
        // freshly written page-table pages to the point of coherency.
        let ap_ttbr0_addr = &AP_TTBR0 as *const AtomicUsize as usize;
        crate::mmu::clean_range_to_poc(ap_ttbr0_addr, 8);
        crate::mmu::clean_range_to_poc(ttbr0, crate::memory::PAGE_SIZE);

        // QEMU virt numbers CPUs by Aff0: cpu index == affinity value.
        let target_mpidr = cpu as u64;
        let entry = ap_entry as *const () as usize;
        let result = psci_cpu_on(target_mpidr, entry);
        crate::console::write_str("SMP: CPU_ON cpu=");
        crate::console::write_dec_u64(cpu as u64);
        crate::console::write_str(" -> ");
        crate::console::write_dec_u64(result as u64);
        crate::console::write_str("\r\n");
        if result != 0 {
            crate::console::write_str("SMP: CPU_ON failed, CPU left offline\r\n");
            return;
        }
    }
}

/// PSCI CPU_ON (SMC32 function id 0x84020002) over the HVC conduit.
/// Returns the PSCI status: 0 = SUCCESS.
fn psci_cpu_on(target_mpidr: u64, entry_physical: usize) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inlateout("x0") 0x8402_0002_u64 => ret,
            in("x1") target_mpidr,
            in("x2") entry_physical as u64,
            in("x3") 0_u64,
            options(nostack),
        );
    }
    ret
}
