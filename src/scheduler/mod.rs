use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::{string::String, vec::Vec};

use silver_microkernel::mem_policy;
use silver_microkernel::sched_policy;
use silver_microkernel::table_registry::TableToken;

use crate::{
    capability,
    condvar::Condvar,
    console,
    elf::LoadedImage,
    elf_loader, frame_allocator, interrupts, kernel_stack,
    linux_elf::LinuxImage,
    memory::{phys_to_virt, virt_to_phys, MemoryRegion, PAGE_SIZE},
    mmu, mutex, smp, timer, user_program, vfs,
    wait_queue::WaitQueue,
};

/// Scheduler self-tests and stress harnesses (`scheduler/selftest.rs`).
mod task;
mod selftest;
mod wait;
mod spawn;
pub use selftest::*;
pub use wait::*;
pub use spawn::*;
use wait::{has_task_capacity, recover_console, take_zombie};
pub use task::*;
use task::{MmapRegion, SharedMapping};

const TASK_STACK_PAGES: usize = 4;
const TASK_STACK_SIZE: usize = TASK_STACK_PAGES * PAGE_SIZE;
const TASK_USER_STACK_PAGES: usize = mmu::USER_STACK_PAGES;
const TASK_USER_STACK_SIZE: usize = mmu::USER_STACK_SIZE;
const IDLE_TASK_ID: u64 = 0;

/// Test/bisect lever: a value other than `usize::MAX` replaces the per-task
/// frame limit (`mem_policy::task_frame_limit`).
static TASK_LIMIT_OVERRIDE: AtomicUsize = AtomicUsize::new(usize::MAX);

fn task_frame_limit() -> usize {
    let override_value = TASK_LIMIT_OVERRIDE.load(Ordering::Relaxed);
    if override_value == usize::MAX {
        mem_policy::task_frame_limit(frame_allocator::total_pages())
    } else {
        override_value
    }
}

/// AP idle-task ids live above the user id space so spawning them cannot
/// shift SCOUT_ID or any other spawn-order identity.
const AP_IDLE_ID_BASE: u64 = 1 << 40;

/// `smp-selftest` observability: where each dispatch came from. A healthy
/// system dispatches from the per-CPU queue (`local`) or steals a runnable
/// task from a peer; `scan` is the correctness fallback (expected mainly for
/// idle tasks and for a task whose enqueue was missed).
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static READY_LOCAL_PICKS: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static READY_STEAL_PICKS: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static READY_SCAN_PICKS: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static READY_PUSH_PICKS: [AtomicU64; smp::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
AtomicU64::new(0),
];
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static READY_QUEUES_REPORTED: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn note_pick(counter: &[AtomicU64; smp::MAX_CPUS], cpu: usize) {
    counter[cpu.min(smp::MAX_CPUS - 1)].fetch_add(1, Ordering::Relaxed);
}

/// Aggregate dispatch-source counts `(local, steal, fallback-scan, push)`.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn ready_queue_stats() -> (u64, u64, u64, u64) {
    let sum = |counter: &[AtomicU64; smp::MAX_CPUS]| {
        counter
            .iter()
            .map(|value| value.load(Ordering::Relaxed))
            .sum()
    };
    (
        sum(&READY_LOCAL_PICKS),
        sum(&READY_STEAL_PICKS),
        sum(&READY_SCAN_PICKS),
        sum(&READY_PUSH_PICKS),
    )
}

/// Prints the dispatch-source mix once, after enough picks have happened to be
/// meaningful. The queues are a cache with a fallback, so this is evidence
/// that the fast path is actually used — not a pass/fail gate.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn report_ready_queues_once() {
    if READY_QUEUES_REPORTED.load(Ordering::Relaxed) {
        return;
    }
    let (local, steal, scan, push) = ready_queue_stats();
    if local + steal + scan < 64 {
        return;
    }
    READY_QUEUES_REPORTED.store(true, Ordering::Relaxed);
    console::write_str("SMP SELFTEST: ready queues local=");
    console::write_dec_u64(local);
    console::write_str(" steal=");
    console::write_dec_u64(steal);
    console::write_str(" scan=");
    console::write_dec_u64(scan);
    console::write_str(" push=");
    console::write_dec_u64(push);
    console::write_str("\r\n");
}

/// How a blocked task is linked back to the wait queue it sleeps in.
///
/// The split makes every unlink provably safe, including from reap:
///
/// * [`WaitLink::Immortal`] queues are owned by kernel objects that are never
///   dropped (`static`s and `MutexState`s) — the same trust level the task
///   already carries in `owned_mutexes`. Unlinking is safe at any time.
/// * [`WaitLink::Shared`] queues are refcounted mailbox condvars. Holding the
///   `Arc` keeps the allocation alive, so unlinking stays safe even after the
///   owning mailbox dropped its reference — a freed-then-dereferenced queue
///   is no longer representable in this type.
pub enum WaitLink {
    Immortal(*const WaitQueue),
    Shared(Arc<Condvar>),
}

/// One queued task in a CPU's ready queue.
///
/// `priority` is the task's class at enqueue time; `age` counts dispatch
/// attempts spent waiting. The age is what gives a low-class task a bounded
/// waiting time (see `sched_policy`).
#[derive(Copy, Clone)]
struct ReadyEntry {
    id: u64,
    index: usize,
    priority: u8,
    age: u16,
}

pub struct TaskManager {
    tasks: Vec<Task>,
    next_id: u64,
    console_parent: Option<u64>,
    /// Per-CPU FIFO of Ready, **non-idle** tasks as `(id, index)` pairs. This
    /// is a cache of the global `tasks` array, not the sole source of truth:
    /// the stored id is checked against `tasks[index].id` on every dequeue, so
    /// a missed rebuild after an index shift can only discard an entry, never
    /// run the wrong task. `find_next_task` also falls back to a full scan, so
    /// a missed enqueue can delay a task but never strand it. Idle tasks stay
    /// out of the queues so the "real task before idle" policy is unchanged.
    ///
    /// FIFO is a correctness property, not a hint: with a stack-shaped queue
    /// (push/pop) and three or more runnable tasks, the oldest entry was never
    /// dispatched. Push at the back; dispatch selects the FIFO-first entry of
    /// the best effective class (priority plus aging).
    ///
    /// Indices are invalidated by any `tasks.remove`/`retain`; those paths
    /// rebuild every queue (see [`rebuild_ready_queues`]).
    ready: [VecDeque<ReadyEntry>; smp::MAX_CPUS],
}

const TRACE_LEN: usize = 16;
static mut SWITCH_TRACE: [(u64, u64, u64, u64); TRACE_LEN] = [(0, 0, 0, 0); TRACE_LEN];
static mut TRACE_POS: usize = 0;

fn trace_switch(from: u64, to: u64, sp: u64, elr: u64) {
    unsafe {
        let pos = core::ptr::addr_of_mut!(TRACE_POS).read();
        core::ptr::addr_of_mut!(SWITCH_TRACE)
            .cast::<(u64, u64, u64, u64)>()
            .add(pos)
            .write((from, to, sp, elr));
        core::ptr::addr_of_mut!(TRACE_POS).write((pos + 1) % TRACE_LEN);
    }
}

pub fn dump_switch_trace() {
    console::write_str("SWITCH TRACE from,to,sp,elr:\r\n");
    unsafe {
        let pos = core::ptr::addr_of!(TRACE_POS).read();
        for offset in 0..TRACE_LEN {
            let index = (pos + offset) % TRACE_LEN;
            let (from, to, sp, elr) = core::ptr::addr_of!(SWITCH_TRACE)
                .cast::<(u64, u64, u64, u64)>()
                .add(index)
                .read();
            if from == 0 && to == 0 {
                continue;
            }
            console::write_dec_u64(from);
            console::write_str("->");
            console::write_dec_u64(to);
            console::write_str(" sp=");
            console::write_hex_u64(sp);
            console::write_str(" elr=");
            console::write_hex_u64(elr);
            console::write_str("\r\n");
        }
    }
}

struct SharedScheduler(UnsafeCell<Option<TaskManager>>);
unsafe impl Sync for SharedScheduler {}

static SCHEDULER: SharedScheduler = SharedScheduler(UnsafeCell::new(None));
static PROCESS_WAIT_QUEUE: WaitQueue = WaitQueue::new();
static mut BOOT_CONTEXT: TaskContext = TaskContext::zero();

/// Per-CPU parking contexts for the very first `schedule` on an AP: the AP
/// enters from its own boot stack, which is never restored once it switches
/// into a real task. One per CPU so parallel APs never share the slot.
static mut AP_DUMMY_CONTEXTS: [TaskContext; smp::MAX_CPUS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: TaskContext = TaskContext::zero();
    [ZERO; smp::MAX_CPUS]
};

/// One-shot trigger: the secondary CPUs are powered on from the first
/// scheduling pass, i.e. strictly after the boot CPU entered its first task
/// and before any user task can observe a half-initialized scheduler.
static AP_BOOT_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// True while this CPU is executing a scheduling pass *outside* the final
/// context switch. A pass mutates a snapshot of every CPU's current index and
/// then republishes it; a timer IRQ that enters `schedule` on the same CPU in
/// between would run a second pass over a half-applied transition and could
/// restore a stale index when the outer pass resumes. The AP idle loop is the
/// one caller that enters `schedule` with IRQs enabled, so the pass both masks
/// IRQs and refuses a nested entry (the churn harness deliberately re-enables
/// IRQs while it waits, and that wait must not schedule recursively).
static SCHED_PASS_ACTIVE: [core::sync::atomic::AtomicBool; smp::MAX_CPUS] = [
    core::sync::atomic::AtomicBool::new(false),
    core::sync::atomic::AtomicBool::new(false),
    core::sync::atomic::AtomicBool::new(false),
    core::sync::atomic::AtomicBool::new(false),
core::sync::atomic::AtomicBool::new(false),
core::sync::atomic::AtomicBool::new(false),
core::sync::atomic::AtomicBool::new(false),
core::sync::atomic::AtomicBool::new(false),
];

/// Set once `init` has published the first task and `set_current_task`.
///
/// Until then there is no valid current task: a timer IRQ can fire inside
/// `scheduler::init` because the PL011 TX wait temporarily unmasks IRQs, and
/// the clamp in `schedule` would mistake idle for the running task and save the
/// boot stack into idle's context. `schedule` must be a no-op before this flag.
static SCHEDULER_READY: AtomicBool = AtomicBool::new(false);

/// Set by `start()` once the first task is marked `Running` on CPU 0.
///
/// Between `finish_init` (`SCHEDULER_READY`) and `start`'s first switch the
/// documented boot window allows a timer IRQ to enter `schedule` while
/// `current_index` already names idle but `running_on` is still unset. The
/// invariant audit must not report that window as a scheduling violation.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
static SCHEDULER_STARTED: AtomicBool = AtomicBool::new(false);

/// Data frames of shared-DMA ranges whose revoke scan could not prove the
/// absence of foreign mappings (see `reap_dead_tasks`): deliberately leaked
/// instead of risking a use-after-free through a live foreign mapping.
static DMA_LEAKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// mmap runs that could not be unmapped completely: the backing frames are
/// leaked instead of freed, because a still-live PTE must never point at a
/// frame the allocator can hand out again.
static MMAP_LEAKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// DMA ranges whose PTEs are gone but whose frames stay allocated for a grace
/// window, so a device queue that was still draining when its task died cannot
/// write into a frame the allocator has already handed to a new owner.
///
/// This is a bounded kernel-side mitigation, **not** a device-quiescence
/// proof: a driver that owns a device must still stop or reset its queue
/// before it exits. The window turns "freed the instant the last PTE vanished"
/// into "recycled only after several scheduling epochs", which covers the
/// common case of an in-flight descriptor being consumed.
const DMA_QUARANTINE_SLOTS: usize = 32;
/// Reclaim passes a range waits before it can be freed. Scheduling runs one
/// pass per timer tick (~10 ms), so the window is roughly 80 ms.
const DMA_QUARANTINE_EPOCHS: u64 = 8;

#[derive(Copy, Clone)]
struct DmaQuarantineEntry {
    physical: usize,
    page_count: usize,
    generation: u32,
    released_epoch: u64,
}

impl DmaQuarantineEntry {
    const EMPTY: Self = Self {
        physical: 0,
        page_count: 0,
        generation: 0,
        released_epoch: 0,
    };
}

static DMA_QUARANTINE: smp::SpinLock<[DmaQuarantineEntry; DMA_QUARANTINE_SLOTS]> =
    smp::SpinLock::new([DmaQuarantineEntry::EMPTY; DMA_QUARANTINE_SLOTS]);
/// Monotonic reclaim-pass counter; incremented by every `reap_dead_tasks`
/// call, including passes that find nothing to free.
static DMA_QUARANTINE_EPOCH: AtomicU64 = AtomicU64::new(0);
/// Frames currently held (not yet reclaimed or leaked) by the quarantine.
static DMA_QUARANTINED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Queues a revoked DMA run for delayed release. Returns `false` when every
/// slot is taken; the caller must then fall back to the existing leak policy.
fn quarantine_dma_run(physical: usize, page_count: usize, generation: u32) -> bool {
    let epoch = DMA_QUARANTINE_EPOCH.load(Ordering::Acquire);
    let mut slots = DMA_QUARANTINE.lock();
    let Some(slot) = slots.iter_mut().find(|slot| slot.page_count == 0) else {
        return false;
    };
    *slot = DmaQuarantineEntry {
        physical,
        page_count,
        generation,
        released_epoch: epoch,
    };
    DMA_QUARANTINED_FRAMES.fetch_add(page_count, Ordering::Relaxed);
    true
}


// ---------------------------------------------------------------------------
// In-flight DMA lease contract
// ---------------------------------------------------------------------------

static DMA_LEASES: smp::SpinLock<silver_microkernel::DmaLeaseTable> =
    smp::SpinLock::new(silver_microkernel::DmaLeaseTable::new());

/// RAII claim that a device may still DMA into `[physical, physical + len)`.
///
/// An in-kernel driver arms a lease *before* handing the buffer to the device
/// and drops it only after observing completion and stopping the queue.
/// [`dma_range_in_flight`] is consulted by the revoke path: a live claim makes
/// the range leak instead of being quarantined and reused under a running
/// device. Userspace drivers cannot arm a lease yet (that needs a syscall), so
/// their ranges still rely on the quarantine window alone.
pub struct DmaLease {
    slot: usize,
}

impl DmaLease {
    /// Claims the range. Returns `None` when the table is full or the range is
    /// empty/overflowing; the caller must then not hand the buffer to a device.
    pub fn arm(physical: usize, byte_len: usize) -> Option<Self> {
        let slot = DMA_LEASES.lock().arm(physical, byte_len)?;
        Some(Self { slot })
    }
}

impl Drop for DmaLease {
    fn drop(&mut self) {
        DMA_LEASES.lock().release(self.slot);
    }
}

/// True while any armed lease overlaps the range.
pub fn dma_range_in_flight(physical: usize, byte_len: usize) -> bool {
    DMA_LEASES.lock().range_busy(physical, byte_len)
}


// ---------------------------------------------------------------------------
// SMP churn stress (selftest)
// ---------------------------------------------------------------------------







/// Frees quarantined runs whose grace window has elapsed. Runs with the kernel
/// lock held from `reap_dead_tasks`; the frame-allocator lock is taken only
/// after the quarantine lock has been released, so no inner-lock ordering
/// cycle is introduced.
fn reclaim_quarantined_dma() {
    let epoch = DMA_QUARANTINE_EPOCH
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    loop {
        let entry = {
            let mut slots = DMA_QUARANTINE.lock();
            let Some(index) = slots.iter().position(|slot| {
                slot.page_count != 0
                    && epoch.saturating_sub(slot.released_epoch) >= DMA_QUARANTINE_EPOCHS
            }) else {
                return;
            };
            let entry = slots[index];
            slots[index] = DmaQuarantineEntry::EMPTY;
            entry
        };
        DMA_QUARANTINED_FRAMES.fetch_sub(entry.page_count, Ordering::Relaxed);
        if !frame_allocator::free_contiguous_pages_tagged(
            entry.physical,
            entry.page_count,
            entry.generation,
        ) {
            // The tag no longer matches (reallocated frame or accounting bug):
            // never free blindly, count the leak instead.
            DMA_LEAKED_FRAMES.fetch_add(entry.page_count, Ordering::Relaxed);
            console::write_str("DMA QUARANTINE: release refused for 0x");
            console::write_hex_u64(entry.physical as u64);
            console::write_str(" (");
            console::write_dec_u64(entry.page_count as u64);
            console::write_str(" pages)\r\n");
        }
    }
}

core::arch::global_asm!(
    r#"
    .section .text.scheduler, "ax"
    .global silver_context_switch
    .type silver_context_switch, %function
silver_context_switch:
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    mov x9, sp
    str x9, [x0, #96]
    mrs x9, sp_el0
    str x9, [x0, #104]
    str x30, [x0, #112]
    mrs x9, tpidr_el0
    str x9, [x0, #120]

    ldr x9, [x1, #120]
    msr tpidr_el0, x9
    ldr x9, [x1, #104]
    msr sp_el0, x9
    ldr x9, [x1, #96]
    mov sp, x9
    ldp x19, x20, [x1, #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    ldr x30, [x1, #112]
    ret
    .size silver_context_switch, . - silver_context_switch

    .global silver_user_bootstrap
    .type silver_user_bootstrap, %function
silver_user_bootstrap:
    // A first dispatch inherits one kernel-lock hold from the dispatching
    // `schedule`/`start` acquire: drop it before entering user mode so the
    // acquire/release pair stays balanced (see the locking contract in smp.rs).
    // `bkl_release_asm` preserves x19/x20 (callee-saved), and the register
    // reads below happen after the call.
    bl {bkl_release}
    mrs x9, tpidr_el1
    add x9, x9, {fp_offset}
    mov x0, x9
    bl silver_fp_restore
    mov x0, x20
    msr elr_el1, x19
    mov x9, xzr
    msr spsr_el1, x9
    eret
    .size silver_user_bootstrap, . - silver_user_bootstrap

    .global silver_fork_bootstrap
    .type silver_fork_bootstrap, %function
silver_fork_bootstrap:
    // A forked child resumes as if its `clone` syscall had just returned.
    // `sp` points at a 272-byte copy of the parent's SVC frame (x0-x30, ELR,
    // SPSR); the copy's x0 is zero and SP_EL0 was set by the context switch.
    bl {bkl_release}
    ldr x9, [sp, #248]
    msr elr_el1, x9
    ldr x9, [sp, #256]
    msr spsr_el1, x9
    mrs x9, tpidr_el1
    add x9, x9, {fp_offset}
    mov x0, x9
    bl silver_fp_restore
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    eret
    .size silver_fork_bootstrap, . - silver_fork_bootstrap
"#,
    bkl_release = sym crate::smp::bkl_release_asm,
    fp_offset = const crate::smp::FP_OFFSET
);

extern "C" {
    fn silver_context_switch(current: *mut TaskContext, next: *const TaskContext);
    fn silver_user_bootstrap();
    #[cfg(feature = "linux-abi")]
    fn silver_fork_bootstrap();
}

pub fn init() {
    let slot = scheduler_slot();
    *slot = Some(TaskManager {
        tasks: Vec::new(),
        next_id: 0,
        console_parent: None,
        ready: core::array::from_fn(|_| VecDeque::new()),
    });

    let _idle_id = spawn_idle(None);

    #[cfg(not(feature = "demo-tasks"))]
    {
        let silverd_id = spawn_user(user_program::SILVERD, 0)
            .expect("silverd must be created at boot");
        let _ = set_console_owner(silverd_id);
        crate::boot_metrics::log_milestone("SILVERD spawn");
    }

    #[cfg(feature = "demo-tasks")]
    {
        let _scout_id = spawn_user(user_program::SCOUT, 0);
        for role in 2..=16 {
            let _ = spawn_user(user_program::DEMO, role);
        }
        let _lifecycle_supervisor = spawn_user(user_program::DEMO, 17);
        let _copyout_probe = spawn_user(user_program::DEMO, 19);
        let _heap_probe = spawn_user(user_program::DEMO, 20);
        let _malloc_probe = spawn_user(user_program::DEMO, 21);
        let _global_alloc_probe = spawn_user(user_program::DEMO, 22);
        for role in 23..=26 {
            let _ = spawn_user(user_program::DEMO, role);
        }
        let _security_probe = spawn_user(user_program::DEMO, 27);
        let _process_api_probe = spawn_user(user_program::DEMO, 29);
        let _mount_denied_probe = spawn_user(user_program::DEMO, 31);
        // Demo harness: provision the privileged mount probe exactly like the
        // system manager in a normal boot. Only this probe receives CAP_MOUNT;
        // every other demo task stays unprivileged.
        if let Some(id) = spawn_user(user_program::DEMO, 32) {
            if let Some(task) = scheduler_manager()
                .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == id))
            {
                task.cap_table.insert(id, capability::CAP_MOUNT);
            }
        }
        let _memory_probe = spawn_user(user_program::DEMO, 34);
        let _power_denied_probe = spawn_user(user_program::DEMO, 37);
        // The resource probe also checks the shared VFS budget, so it needs the
        // trusted mount right in addition to the normal demo capabilities.
        if let Some(id) = spawn_user(user_program::DEMO, 39) {
            if let Some(task) = scheduler_manager()
                .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == id))
            {
                task.cap_table.insert(id, capability::CAP_MOUNT);
            }
        }
        // MMIO/DMA probe: trusted CAP_MMIO is granted exactly like a device
        // driver would receive it; the probe delegates it only to its own
        // short-lived DMA writer/reader children.
        if let Some(id) = spawn_user(user_program::DEMO, 40) {
            if let Some(task) = scheduler_manager()
                .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == id))
            {
                task.cap_table.insert(id, capability::CAP_MMIO);
                // The capacity probe maps 0x0010_0000 as a device range; give
                // it exactly that range, like a real driver's DT node.
                task.cap_table.insert_region(
                    id,
                    capability::CAP_MMIO,
                    Some((0x0010_0000, 0x1000)),
                );
            }
        }
        let _zombie_probe = spawn_user(user_program::DEMO, 43);
        let _elf_lifecycle_probe = spawn_user(user_program::DEMO, 45);
        // Nameserver liveness probe: provision a CAP_KILL handle on Scout so a
        // controlled death can be induced. The probe itself runs after all
        // normal nameserver users have resolved their peers.
        if let Some(id) = spawn_user(user_program::DEMO, 46) {
            if let Some(task) = scheduler_manager()
                .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == id))
            {
                task.cap_table
                    .insert(crate::syscall::SCOUT_ID, capability::CAP_KILL);
            }
        }
        let _fs_read_probe = spawn_user(user_program::DEMO, 48);
        let _mmap_wx_probe = spawn_user(user_program::DEMO, 49);
        // Demo harness: the only task allowed to power the VM off receives
        // CAP_POWER explicitly, matching the trusted graphics driver in a
        // normal boot. Every other demo task stays unprivileged.
        if let Some(id) = spawn_user(user_program::DEMO, 38) {
            if let Some(task) = scheduler_manager()
                .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == id))
            {
                task.cap_table.insert(id, capability::CAP_POWER);
            }
        }
    }

    finish_init(0);
}

/// Fog PoC boot: idle, the nameserver and Howl. No virtio drivers, no
/// silverd, no disk/network/GPU probes — none of them exist on a bare phone
/// first boot, and each failed probe would only add delay before the prompt.
/// Scout must start before Howl because `SYS_REGISTER` resolves the name
/// through the nameserver.
#[cfg(feature = "fog")]
pub fn init_fog() {
    let slot = scheduler_slot();
    *slot = Some(TaskManager {
        tasks: Vec::new(),
        next_id: 0,
        console_parent: None,
        ready: core::array::from_fn(|_| VecDeque::new()),
    });

    let _idle_id = spawn_idle(None);
    let _scout_id = spawn_user(user_program::SCOUT, 0).expect("scout must start on fog");
    let howl_id = spawn_user(user_program::HOWL, 0).expect("howl must start on fog");
    let _ = set_console_owner(howl_id);

    // Experimental phone UI: the compositor takes the bootloader's registered
    // panel buffer and SilverHome draws through the command stream. Input is
    // not spawned until a real touch driver exists.
    finish_init(0);
}

fn finish_init(first_id: u64) {
    // AP idle tasks are appended after every boot task, preserving SCOUT_ID
    // and the other spawn-order identities.
    for cpu in 1..smp::cpu_count() {
        let _ = spawn_idle(Some(AP_IDLE_ID_BASE + cpu as u64));
    }
    let manager = scheduler_manager().unwrap();
    // Per-CPU "current": the boot CPU starts on the first spawned task; APs
    // begin at no-task and pick one in their own first `schedule`.
    let first_index = manager
        .tasks
        .iter()
        .position(|task| task.id == first_id)
        .unwrap_or(0);
    // Publish the id that actually sits at the resolved index: `first_id == 0`
    // is the "not spawned yet" sentinel and falls back to index 0 (the idle
    // task), whose real id is what the lock-free diagnostic should report.
    let first_task_id = manager
        .tasks
        .get(first_index)
        .map(|task| task.id)
        .unwrap_or(0);
    smp::set_current_task(first_index, first_task_id);
    // Run the boot proofs BEFORE `SCHEDULER_READY`: console writes re-enable
    // IRQs while waiting for FIFO space, so a timer IRQ can reach `schedule()`
    // in the middle of a selftest. With the scheduler not ready that call is a
    // no-op; otherwise it would boot the APs and run the whole SMP selftest
    // suite underneath these checks.
    if !run_task_ledger_selftest() {
        panic!("task ledger selftest failed");
    }
    if !run_mmap_split_selftest() {
        panic!("mmap split selftest failed");
    }
    // From here on `schedule` may run: the current index names a real task.
    SCHEDULER_READY.store(true, Ordering::Release);
}


fn current_task_mmap_regions() -> usize {
    current_task_mut()
        .map(|task| task.mmap_regions.len())
        .unwrap_or(0)
}

/// Current task's charged frame count.
pub fn current_task_mem_frames() -> Option<usize> {
    current_task_mut().map(|task| task.mem_frames)
}

/// Charges `count` frames to the running task's ledger.
pub fn charge_current_frames(count: usize) -> bool {
    current_task_mut().map_or(false, |task| task.charge_frames(count))
}

/// Releases `count` frames from the running task's ledger.
pub fn release_current_frames(count: usize) {
    if let Some(task) = current_task_mut() {
        task.release_frames(count);
    }
}





/// Kernel-only scheduling class setter; there is no syscall for it (userspace
/// tasks are always Normal). Kernel policy and the boot selftest use it.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn set_task_priority(id: u64, priority: u8) -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) else {
        return false;
    };
    task.priority = sched_policy::normalize(priority);
    // Queues cache the class captured at enqueue time, so a change must
    // rebuild them to take effect on the next dispatch.
    rebuild_ready_queues(manager);
    true
}





/// Runs one blocking scheduling step for a caller that does not necessarily
/// hold the kernel lock yet. User tasks reach `yield_current`/`sleep` through
/// a syscall, which holds the lock; a kernel task entry drops the inherited
/// hold before it starts running, so it must re-acquire one for the step.
/// Holding the lock while `schedule_released` releases and re-acquires it
/// keeps the lock depth balanced in both cases.
fn schedule_step() {
    let acquired = smp::bkl_depth() == 0;
    if acquired {
        smp::bkl_acquire();
    }
    schedule_released();
    if acquired {
        smp::bkl_release();
    }
}

pub fn yield_current() {
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        if task.state == TaskState::Running {
            task.state = TaskState::Ready;
        }
    }

    schedule_step();
    interrupts::restore_irq(saved_daif);
}

/// Applies a blocking/ready transition unless the task has already been
/// killed. `exit_group` marks siblings Dead from another CPU; a task that is
/// in the middle of a syscall would otherwise overwrite Dead with Sleeping or
/// Blocked and never be reaped.
fn set_state_unless_dead(task: &mut Task, next: TaskState) {
    if !matches!(task.state, TaskState::Dead | TaskState::Zombie) {
        task.state = next;
    }
}

/// Drops a task's IRQ binding and masks the line in the GIC. Every path that
/// clears `bound_irq` must go through here: leaving the line enabled lets the
/// next hardware interrupt take the unknown-INTID path with no owner.
fn release_bound_irq(task: &mut Task) {
    if let Some(intid) = task.bound_irq {
        crate::gic::disable_irq(intid);
        task.bound_irq = None;
    }
    // A dying driver must not keep PCI BAR claims (SYS_MMIO_CLAIM) alive.
    crate::driver_resources::release_mmio_claims(task.id);
}

pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        yield_current();
        return;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let wake_at = timer::ticks().saturating_add(timer::milliseconds_to_ticks(milliseconds));
    if let Some(task) = current_task_mut() {
        set_state_unless_dead(task, TaskState::Sleeping(wake_at));
    }
    schedule_step();
    interrupts::restore_irq(saved_daif);
}

pub fn wait_irq() -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    if current_task_mut().and_then(|task| task.bound_irq).is_none() {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    loop {
        let Some(task) = current_task_mut() else {
            interrupts::restore_irq(saved_daif);
            return false;
        };
        if task.irq_pending {
            task.irq_pending = false;
            if let Some(intid) = task.bound_irq {
                crate::gic::enable_irq(intid);
            }
            interrupts::restore_irq(saved_daif);
            return true;
        }
        if let Some(intid) = task.bound_irq {
            crate::gic::enable_irq(intid);
        }
        set_state_unless_dead(task, TaskState::Blocked);
        schedule_released();
    }
}

pub fn irq_is_bound(intid: u32) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq == Some(intid)
            })
        })
        .unwrap_or(false)
}

pub fn wake_bound_irq(intid: u32) -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| {
        !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq == Some(intid)
    }) else {
        return false;
    };
    manager.tasks[index].irq_pending = true;
    wake_blocked_index(manager, index, smp::this_cpu_id());
    true
}

/// Task that owns the PL011 interrupt, i.e. the userspace console sink.
///
/// Resolved from live scheduler state instead of a hardcoded task ID: the
/// binding of INTID 33 is exclusive, so at most one task can be the sink, and
/// console routing then survives any spawn order chosen by silverd or the shell.
#[cfg(not(feature = "demo-tasks"))]
pub fn console_sink_task() -> Option<u64> {
    let saved_daif = interrupts::save_and_disable_irq();
    let sink = scheduler_manager().and_then(|manager| {
        manager
            .tasks
            .iter()
            .find(|task| {
                !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq == Some(crate::gic::uart_irq())
            })
            .map(|task| task.id)
    });
    interrupts::restore_irq(saved_daif);
    sink
}

#[cfg(not(feature = "demo-tasks"))]
pub fn notify_bound_task(id: u64) -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    let notified = scheduler_manager()
        .and_then(|manager| {
            let index = manager.tasks.iter().position(|task| {
                task.id == id
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq.is_some()
            })?;
            manager.tasks[index].irq_pending = true;
            wake_blocked_index(manager, index, smp::this_cpu_id());
            Some(())
        })
        .is_some();
    interrupts::restore_irq(saved_daif);
    notified
}

pub fn block_current_on(queue: &WaitQueue, link: WaitLink) {
    crate::smp::probe(4);
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(link);
        set_state_unless_dead(task, TaskState::Blocked);
    }
    schedule_released();
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn block_current_on_until(queue: &WaitQueue, link: WaitLink, deadline: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(link);
        set_state_unless_dead(task, TaskState::BlockedUntil(deadline));
    }
    schedule_released();
    queue.remove(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn wake_task(id: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    smp::bkl_acquire();
    if let Some(manager) = scheduler_manager() {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            wake_blocked_index(manager, index, smp::this_cpu_id());
        }
    }
    smp::bkl_release();
    interrupts::restore_irq(saved_daif);
    // The woken task may be pickable by a CPU that is currently idle or
    // running something else: kick the others to re-evaluate their queues.
    smp::ipi_reschedule_others();
}

/// Wakes a task so a pending signal can be observed. Unlike [`wake_task`],
/// this also interrupts `TaskState::Sleeping`; the syscall that called
/// `sleep` re-checks its condition after the wake and the SVC return hook
/// delivers the handler.
pub fn wake_for_signal(id: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    smp::bkl_acquire();
    let mut woke = false;
    if let Some(manager) = scheduler_manager() {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            let task = &mut manager.tasks[index];
            match task.state {
                TaskState::Sleeping(_) | TaskState::Blocked | TaskState::BlockedUntil(_) => {
                    task.state = TaskState::Ready;
                    task.waiting_on = None;
                    enqueue_ready(manager, smp::this_cpu_id(), index);
                    woke = true;
                }
                _ => {}
            }
        }
    }
    smp::bkl_release();
    interrupts::restore_irq(saved_daif);
    if woke {
        smp::ipi_reschedule_others();
    }
}

pub fn register_mutex(state: *mut mutex::MutexState) {
    if let Some(task) = current_task_mut() {
        if !task.owned_mutexes.contains(&state) {
            task.owned_mutexes.push(state);
        }
    }
}

pub fn unregister_mutex(state: *mut mutex::MutexState) {
    let Some(task) = current_task_mut() else {
        // Early boot has no task context yet (the console lock itself is
        // released there). Printing from this path re-enters the console and
        // floods the UART (~145k lines on one CPU), so stay silent; the
        // stale-state diagnostic below still covers the task-context case.
        return;
    };
    let before = task.owned_mutexes.len();
    task.owned_mutexes.retain(|owned| *owned != state);
    if task.owned_mutexes.len() == before {
        // Unlocking a state this task never registered is an ownership-
        // accounting bug. The diagnostic must stay: this branch used to be
        // provable-unreachable for LLVM, which emitted a bare `b .` (UB) for
        // it — it has to remain real, reachable code.
        crate::console::write_str("MUTEX: unlock of unregistered mutex state\r\n");
    }
}

/// Upper bound on unwaited zombies. Beyond it the oldest zombie record is
/// dropped (its status is lost), bounding metadata growth for a parent that
/// never waits. Executable resources are already released at this point.
const ZOMBIE_LIMIT: usize = 64;

/// Upper bound on *active* task records (everything except a `Zombie`): a
/// `Dead` task still owns its stacks and address space until reap, so it
/// counts. Counting zombies here used to starve spawning permanently: with
/// `ZOMBIE_LIMIT == MAX_TASKS`, a parent that never waits could fill every
/// slot with metadata-only zombies and no new task could start.
pub const MAX_TASKS: usize = 64;

/// Hard bound on the task table, including zombie records. Zombies are capped
/// separately by [`ZOMBIE_LIMIT`], so this is the worst-case metadata bound
/// that keeps the kernel heap finite.
const MAX_TASK_RECORDS: usize = MAX_TASKS + ZOMBIE_LIMIT;

/// Per-CPU snapshot keyed by CPU id, not by position in the online set.
///
/// A compact `Vec` of the online CPUs plus `zip` in the remap was racy: APs
/// publish `online` from their own trampoline without the kernel lock, so a
/// CPU that came online between the snapshot and the remap shifted every
/// later entry by one. Keying the array by CPU id makes the snapshot immune to
/// that, and `None` keeps its literal meaning: "this CPU had no current task".
fn snapshot_current_ids(manager: &TaskManager) -> [Option<u64>; smp::MAX_CPUS] {
    let mut snapshot = [None; smp::MAX_CPUS];
    for cpu in 0..smp::MAX_CPUS {
        if smp::cpu_online(cpu) {
            snapshot[cpu] = manager
                .tasks
                .get(smp::current_index_of(cpu))
                .map(|task| task.id);
        }
    }
    snapshot
}

fn remap_current_indices(
    manager: &TaskManager,
    snapshot: &[Option<u64>; smp::MAX_CPUS],
    live_id: Option<u64>,
) {
    for (cpu, current_id) in snapshot.iter().enumerate() {
        if !smp::cpu_online(cpu) {
            continue;
        }
        let Some(current_id) = current_id else {
            // No current task when the snapshot was taken: an AP parked before
            // its first pick, or a CPU that already had no task. Leave the
            // sentinel alone. Writing an index here would hand the CPU another
            // task; the SP guard would then reject its genuine boot stack and
            // pin it in `wait_for_irq` forever.
            continue;
        };
        // A current task that no longer exists (removed while dead) leaves the
        // CPU with no task, which is `usize::MAX` by contract.
        let index = manager
            .tasks
            .iter()
            .position(|task| task.id == *current_id)
            .unwrap_or(usize::MAX);
        let published = if index == usize::MAX { 0 } else { *current_id };
        smp::set_current_task_for(cpu, index, published);
    }
    // The caller's own CPU may be missing from the snapshot only if it is not
    // `online`; `live_id` remains the authoritative value for it. A `None`
    // live id is the "no current task" sentinel and must not be turned into an
    // index.
    if let Some(id) = live_id {
        let index = manager
            .tasks
            .iter()
            .position(|task| task.id == id)
            .unwrap_or(usize::MAX);
        let published = if index == usize::MAX { 0 } else { id };
        smp::set_current_task(index, published);
    }
}

/// Proves that a reap/removal which remaps every CPU's current index preserves
/// the "no current task" sentinel of an online AP that has not made its first
/// pick. Runs before the APs are released from `ap_tlb_probe_wait`, when their
/// `current_index` is still `usize::MAX`.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn remap_probe_task() -> ! {
    exit_current()
}


/// Read-only cross-CPU scheduling invariant snapshot, taken with the kernel
/// lock held. Every `running_on`/`current_index` transition happens inside a
/// BKL-protected scheduling pass, so the lock makes this a consistent cut.
///
/// A task may legitimately be `Ready` while still recorded as running on its
/// CPU: `block_current_on` releases the lock before its own next pass, and a
/// wake (or timeout) can move it back to `Ready` in that window. The owning CPU
/// then either resumes it or switches away. What must never drift is the
/// *pairing*: `running_on` and the CPU's `current_index` name each other
/// exactly.
///
/// Returns the first violated invariant for a precise selftest failure.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
fn verify_scheduling_invariants(manager: &TaskManager) -> Result<(), &'static str> {
    let tasks = &manager.tasks;
    let mut current = [usize::MAX; smp::MAX_CPUS];
    let mut online = [false; smp::MAX_CPUS];

    for cpu in 0..smp::MAX_CPUS {
        if !smp::cpu_online(cpu) {
            continue;
        }
        online[cpu] = true;
        let index = smp::current_index_of(cpu);
        current[cpu] = index;
        if index == usize::MAX {
            // An AP that has not dispatched a task yet.
            continue;
        }
        let Some(task) = tasks.get(index) else {
            return Err("a CPU's current_index is out of range");
        };
        if task.running_on != Some(cpu) {
            return Err("a CPU's current task is not marked running on it");
        }
    }

    for (index, task) in tasks.iter().enumerate() {
        if task.state == TaskState::Running && task.running_on.is_none() {
            return Err("a Running task names no CPU");
        }
        if let Some(cpu) = task.running_on {
            if cpu >= smp::MAX_CPUS || !online[cpu] {
                return Err("running_on names an offline CPU");
            }
            if current[cpu] != index {
                return Err("running_on does not match the CPU's current task");
            }
        }
    }

    for a in 0..smp::MAX_CPUS {
        if current[a] == usize::MAX {
            continue;
        }
        for b in (a + 1)..smp::MAX_CPUS {
            if current[b] == current[a] {
                return Err("two CPUs name the same current task");
            }
        }
    }

    // A queue entry out of range means an index-shifting removal forgot to
    // rebuild; an id that no longer matches `tasks[index]` means the same drift
    // was not rebuilt. Either would be a dispatch-time memory-safety bug.
    for queue in &manager.ready {
        for entry in queue {
            match tasks.get(entry.index) {
                None => return Err("a ready queue holds an out-of-range index"),
                Some(task) if task.id != entry.id => {
                    return Err("a ready queue entry names the wrong task")
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn wake_process_waiters(manager: &mut TaskManager) {
    let mut woke_any = false;
    while let Some(waiter) = PROCESS_WAIT_QUEUE.pop() {
        let found = manager.tasks.iter().position(|task| {
            task.id == waiter && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
        });
        if let Some(index) = found {
            wake_blocked_index(manager, index, smp::this_cpu_id());
            woke_any = true;
        }
    }
    if woke_any {
        smp::ipi_reschedule_others();
    }
}

fn wake_blocked_index(manager: &mut TaskManager, index: usize, cpu: usize) -> bool {
    let Some(task) = manager.tasks.get_mut(index) else {
        return false;
    };
    let became_ready = match task.state {
        TaskState::Blocked | TaskState::BlockedUntil(_) => {
            task.state = TaskState::Ready;
            task.waiting_on = None;
            true
        }
        TaskState::Stopped
            if matches!(
                task.resume_state,
                Some(TaskState::Blocked | TaskState::BlockedUntil(_))
            ) =>
        {
            task.resume_state = Some(TaskState::Ready);
            task.waiting_on = None;
            false
        }
        _ => false,
    };
    if became_ready {
        // A task still recorded as running on a CPU (its block released the
        // lock, but its own pass has not switched away yet) is not queued: its
        // owning CPU's switch-away path enqueues it. Queuing it here would let
        // another CPU claim a task that is still executing.
        enqueue_ready(manager, cpu, index);
    }
    became_ready
}

pub fn current_task_id() -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.id)
        .unwrap_or(0)
}

pub fn current_tgid() -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.tgid)
        .unwrap_or(0)
}

/// Thread-group leader for `task`: itself for a process, the leader's id for a
/// thread. Shared process state (descriptors, signal dispositions) is keyed by
/// this id.
pub fn group_leader(task: u64) -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.iter().find(|entry| entry.id == task))
        .map(|entry| if entry.is_thread { entry.tgid } else { entry.id })
        .unwrap_or(task)
}

/// True when the current task is a `CLONE_VM` sibling rather than a process
/// leader. Its address space belongs to the leader and must not be replaced
/// (or released) by `execve`.
#[cfg(feature = "linux-abi")]
pub fn current_is_thread() -> bool {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.is_thread)
        .unwrap_or(false)
}

/// Parent task id of `task`, or `None` for a root task.
pub fn task_parent(task: u64) -> Option<u64> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.iter().find(|entry| entry.id == task))
        .and_then(|entry| entry.ppid)
}

/// Personality of a live task, used to bound signals to the Linux layer.
#[cfg(feature = "linux-abi")]
pub fn task_personality(task: u64) -> Option<Personality> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.iter().find(|entry| entry.id == task))
        .map(|entry| entry.personality)
}

/// True when `task` holds a delegated capability for `target` carrying
/// `rights`. Mirrors `SYS_WAITPID`'s authorization: a spawn handle is
/// authority over that specific target, not a global privilege.
pub fn task_has_target_rights(task: u64, target: u64, rights: u32) -> bool {
    scheduler_manager()
        .and_then(|manager| manager.tasks.iter().find(|entry| entry.id == task))
        .map(|entry| entry.cap_table.has_target_rights(target, rights))
        .unwrap_or(false)
}

/// Live user TLS register. `TaskContext::tls` is only refreshed when the task
/// is switched out, so a task that is currently running (and is therefore the
/// caller of `fork`) must read the register itself.
fn current_user_tls() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Task id that owns the descriptor table. Threads share the leader's table.
pub fn fd_table_owner(task: u64) -> u64 {
    group_leader(task)
}

/// Runs `action` on the group leader (the current task for a process, the
/// leader for a thread), so heap/mmap/image operations land in the shared
/// process state.
fn with_group_owner<R>(action: impl FnOnce(&mut Task) -> R) -> Option<R> {
    let owner = current_tgid();
    let manager = scheduler_manager()?;
    manager
        .tasks
        .iter_mut()
        .find(|task| task.id == owner)
        .map(action)
}

/// Syscall ABI of the running task. Kernel tasks (no user image) are Native.
pub fn current_personality() -> Personality {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.personality)
        .unwrap_or(Personality::Native)
}


/// True when an online CPU is currently executing a task in `ttbr0`'s
/// address space. The kernel lock must be held. `ttbr0 == 0` is the kernel
/// context and never owns user tables.
pub fn space_is_running(ttbr0: usize) -> bool {
    if ttbr0 == 0 {
        return false;
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    for cpu in 0..smp::cpu_count() {
        if !smp::cpu_online(cpu) {
            continue;
        }
        let index = smp::current_index_of(cpu);
        let Some(task) = manager.tasks.get(index) else {
            continue;
        };
        if task.state == TaskState::Running
            && task.running_on == Some(cpu)
            && mmu::root_of(task.ttbr0) == mmu::root_of(ttbr0)
        {
            return true;
        }
    }
    false
}

/// Parent PID of the calling task, or `None` for boot-spawned tasks.
pub fn current_task_ppid() -> Option<u64> {
    current_task_mut().and_then(|task| task.ppid)
}



/// Linux `getpid`: PID 1 for an init task, the thread-group id otherwise.
pub fn current_linux_pid() -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| if task.pid_one { 1 } else { task.tgid })
        .unwrap_or(0)
}

/// Linux `getppid`: a child of the init task reports 1, init reports 0.
pub fn current_linux_ppid() -> u64 {
    let Some(parent) = current_task_ppid() else {
        return 0;
    };
    let manager = match scheduler_manager() {
        Some(manager) => manager,
        None => return 0,
    };
    manager
        .tasks
        .iter()
        .find(|task| task.id == parent)
        .map(|task| if task.pid_one { 1 } else { task.tgid })
        .unwrap_or(0)
}

pub fn current_task_owns_console() -> bool {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.owns_console)
        .unwrap_or(false)
}

pub fn stop_console_owner() -> bool {
    let _bkl = smp::BklGuard::new();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut stopped = false;
    if let Some(manager) = scheduler_manager() {
        if manager.console_parent.is_some() {
            if let Some(task) = manager.tasks.iter_mut().find(|task| {
                task.owns_console && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            }) {
                let id = task.id;
                let previous = if task.state == TaskState::Running {
                    TaskState::Ready
                } else {
                    task.state
                };
                task.resume_state = Some(previous);
                task.state = TaskState::Stopped;
                recover_console(manager, id);
                wake_process_waiters(manager);
                stopped = true;
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    stopped
}

pub fn resume_task(id: u64) -> bool {
    if id == IDLE_TASK_ID {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager()
        .map(|manager| {
            let Some(index) = manager.tasks.iter().position(|task| {
                task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            }) else {
                return false;
            };
            {
                let task = &mut manager.tasks[index];
                if task.state == TaskState::Stopped {
                    task.state = task.resume_state.take().unwrap_or(TaskState::Ready);
                    if task.state == TaskState::Running {
                        task.state = TaskState::Ready;
                    }
                }
            }
            enqueue_ready(manager, smp::this_cpu_id(), index);
            true
        })
        .unwrap_or(false);
    interrupts::restore_irq(saved_daif);
    result
}

pub fn set_console_owner(id: u64) -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    let caller = current_task_id();
    let result = scheduler_manager()
        .map(|manager| {
            let valid = id != IDLE_TASK_ID
                && manager.tasks.iter().any(|task| {
                    task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                });
            if valid {
                for task in &mut manager.tasks {
                    task.owns_console = task.id == id;
                }
                manager.console_parent = if id == caller || caller == IDLE_TASK_ID {
                    None
                } else {
                    Some(caller)
                };
            }
            valid
        })
        .unwrap_or(false);
    interrupts::restore_irq(saved_daif);
    result
}

pub fn task_alive(id: u64) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            })
        })
        .unwrap_or(false)
}

/// Copies the current task's `argv` into `buffer` without allocating, for
/// fault dumps: `argv[0] argv[1] ...` up to the buffer size. Returns the
/// number of bytes written.
pub fn current_task_argv_prefix(buffer: &mut [u8]) -> usize {
    let mut length = 0;
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| {
            for argument in &task.argv {
                if length >= buffer.len() {
                    break;
                }
                if length > 0 && length < buffer.len() {
                    buffer[length] = b' ';
                    length += 1;
                }
                for byte in argument.iter() {
                    if length >= buffer.len() {
                        break;
                    }
                    buffer[length] = *byte;
                    length += 1;
                }
            }
        });
    length
}

/// Prints the current task's virtual memory map to the console without
/// allocating: every mmap region (with lazy/shared/file flags), the brk heap,
/// the stack and the Linux image runs. Called from the fault dump — an
/// overlap between two of these lines is the smoking gun for a VA collision.
pub fn dump_current_task_map() {
    let mut line = [0_u8; 160];
    let mut push = |line: &mut [u8], length: &mut usize, text: &[u8]| {
        let take = text.len().min(line.len() - *length);
        line[*length..*length + take].copy_from_slice(&text[..take]);
        *length += take;
    };
    if let Some(result) = scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| {
            let mut total = 0usize;
            crate::console::write_str("MAP regions:\r\n");
            for region in &task.mmap_regions {
                total += 1;
                let mut length = 0;
                push(&mut line, &mut length, b"MAP va=0x");
                crate::console::write_bytes(&line[..length]);
                crate::console::write_hex_u64(region.virtual_address as u64);
                let mut length = 0;
                push(&mut line, &mut length, b" pages=");
                crate::console::write_bytes(&line[..length]);
                crate::console::write_dec_u64(region.page_count as u64);
                let mut length = 0;
                push(
                    &mut line,
                    &mut length,
                    if region.prot & crate::mmu::PROT_WRITE != 0 {
                        b" rw"
                    } else {
                        b" r-"
                    },
                );
                if region.prot & crate::mmu::PROT_EXEC != 0 {
                    push(&mut line, &mut length, b"x");
                }
                if region.lazy {
                    push(&mut line, &mut length, b" lazy(");
                    crate::console::write_bytes(&line[..length]);
                    crate::console::write_dec_u64(region.pages.len() as u64);
                    let mut length = 0;
                    push(&mut line, &mut length, b" touched)");
                    crate::console::write_bytes(&line[..length]);
                }
                if region.shared.is_some() {
                    let mut length = 0;
                    push(&mut line, &mut length, b" MEMFD-WINDOW");
                    crate::console::write_bytes(&line[..length]);
                }
                if let Some((path, offset)) = &region.file {
                    let mut length = 0;
                    push(&mut line, &mut length, b" file=\"");
                    crate::console::write_bytes(&line[..length]);
                    crate::console::write_str(path);
                    let mut length = 0;
                    push(&mut line, &mut length, b"\" off=");
                    crate::console::write_bytes(&line[..length]);
                    crate::console::write_dec_u64(*offset as u64);
                }
                crate::console::write_str("\r\n");
            }
            crate::console::write_str("HEAP va=0x");
            crate::console::write_hex_u64(mmu::USER_HEAP_VA as u64);
            crate::console::write_str(" pages=");
            crate::console::write_dec_u64(task.heap_pages.len() as u64);
            crate::console::write_str(" break=0x");
            crate::console::write_hex_u64(task.program_break as u64);
            crate::console::write_str("\r\nSTACK va=0x");
            crate::console::write_hex_u64(task.user_stack.start as u64);
            crate::console::write_str("\r\n");
            #[cfg(feature = "linux-abi")]
            #[cfg(feature = "linux-abi")]
            if let Some(image) = &task.linux_image {
                crate::console::write_str("IMAGE runs=");
                crate::console::write_dec_u64(image.runs.len() as u64);
                crate::console::write_str(" top=0x");
                crate::console::write_hex_u64(image.upper_bound as u64);
                crate::console::write_str("\r\n");
                for run in &image.runs {
                    crate::console::write_str("RUN va=0x");
                    crate::console::write_hex_u64(run.virtual_address as u64);
                    crate::console::write_str(" pages=");
                    crate::console::write_dec_u64(run.page_count as u64);
                    crate::console::write_str("\r\n");
                }
            }
            total
        })
    {
        crate::console::write_str("MAP total=");
        crate::console::write_dec_u64(result as u64);
        crate::console::write_str("\r\n");
    }
}

/// Physical span owned by some mapping: `[start, start + pages*PAGE)`.
#[derive(Clone, Copy)]
struct PhysSpan {
    start: usize,
    pages: usize,
    task_id: u64,
    label: &'static str,
}

/// Bounded, allocation-free physical alias report: walks every task's mmap
/// regions (contiguous runs, touched lazy pages, per-frame file mappings),
/// Linux image runs and stacks, and prints any two mappings that share
/// physical frames. Two mappings on one frame is the smoking gun for the
/// Wayland bytes landing in the wrong object.
pub fn dump_physical_alias_report() {
    const MAX_SPANS: usize = 1024;
    const MAX_PAGES: usize = 16384;
    // Kernel-stack budget: arrays this large live in BSS, not on the
    // exception stack. Two CPUs faulting at once would race on them; the
    // report is a diagnostic, so a garbled line is acceptable.
    static REPORTING: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
    static mut SPAN_STORAGE: [PhysSpan; MAX_SPANS] =
        [PhysSpan { start: 0, pages: 0, task_id: 0, label: "" }; MAX_SPANS];
    static mut PAGE_STORAGE: [PhysSpan; MAX_PAGES] =
        [PhysSpan { start: 0, pages: 0, task_id: 0, label: "" }; MAX_PAGES];
    if REPORTING.swap(true, Ordering::Acquire) {
        return; // another CPU is already printing a report
    }
    let spans = unsafe { &mut *core::ptr::addr_of_mut!(SPAN_STORAGE) };
    let pages = unsafe { &mut *core::ptr::addr_of_mut!(PAGE_STORAGE) };
    let mut span_count = 0usize;
    let mut page_count = 0usize;
    let mut truncated = false;
    let page_size = PAGE_SIZE;
    if scheduler_manager().is_some() {
        for manager in scheduler_manager() {
            for task in &manager.tasks {
                if task.id == 0 {
                    continue;
                }
                let id = task.id;
                if span_count < MAX_SPANS && task.user_stack.start != 0 {
                    spans[span_count] = PhysSpan {
                        start: task.user_stack.start,
                        pages: TASK_USER_STACK_SIZE / page_size,
                        task_id: id,
                        label: "stack",
                    };
                    span_count += 1;
                } else {
                    truncated = true;
                }
                for region in &task.mmap_regions {
                    if region.lazy {
                        for (_, (physical, _)) in region.pages.iter() {
                            if page_count >= MAX_PAGES {
                                truncated = true;
                                break;
                            }
                            pages[page_count] = PhysSpan {
                                start: *physical,
                                pages: 1,
                                task_id: id,
                                label: "lazy",
                            };
                            page_count += 1;
                        }
                        for (physical, _) in &region.frames {
                            if page_count >= MAX_PAGES {
                                truncated = true;
                                break;
                            }
                            pages[page_count] = PhysSpan {
                                start: *physical,
                                pages: 1,
                                task_id: id,
                                label: "frame",
                            };
                            page_count += 1;
                        }
                    } else {
                        if span_count >= MAX_SPANS {
                            truncated = true;
                            break;
                        }
                        if region.physical_start != 0 {
                            spans[span_count] = PhysSpan {
                                start: region.physical_start,
                                pages: region.page_count,
                                task_id: id,
                                label: "mmap",
                            };
                            span_count += 1;
                        }
                    }
                }
                #[cfg(feature = "linux-abi")]
                if let Some(image) = &task.linux_image {
                    for run in &image.runs {
                        if span_count >= MAX_SPANS {
                            truncated = true;
                            break;
                        }
                        spans[span_count] = PhysSpan {
                            start: run.physical_start,
                            pages: run.page_count,
                            task_id: id,
                            label: "run",
                        };
                        span_count += 1;
                    }
                }
            }
        }
    }
    spans[..span_count].sort_by_key(|span| span.start);
    pages[..page_count].sort_by_key(|span| span.start);
    crate::console::write_str("PHYSPANS spans=");
    crate::console::write_dec_u64(span_count as u64);
    crate::console::write_str(" pages=");
    crate::console::write_dec_u64(page_count as u64);
    if truncated {
        crate::console::write_str(" (truncated)");
    }
    crate::console::write_str("\r\n");
    let mut aliases = 0;
    let mut report = |aliases: &mut usize, first: &PhysSpan, second: &PhysSpan| {
        *aliases += 1;
        crate::console::write_str("ALIAS phys=0x");
        crate::console::write_hex_u64(second.start as u64);
        crate::console::write_str(" task");
        crate::console::write_dec_u64(first.task_id);
        crate::console::write_str("(");
        crate::console::write_str(first.label);
        crate::console::write_str(") vs task");
        crate::console::write_dec_u64(second.task_id);
        crate::console::write_str("(");
        crate::console::write_str(second.label);
        crate::console::write_str(")\r\n");
    };
    for index in 1..span_count {
        let previous = &spans[index - 1];
        let current = &spans[index];
        if current.start < previous.start + previous.pages * page_size {
            report(&mut aliases, previous, current);
            if aliases >= 24 {
                break;
            }
        }
    }
    // A single-frame mapping landing inside a contiguous span of any task.
    if aliases < 24 {
        for entry in &pages[..page_count] {
            let mut low = 0usize;
            let mut high = span_count;
            while low < high {
                let middle = (low + high) / 2;
                if spans[middle].start + spans[middle].pages * page_size
                    <= entry.start
                {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            if low < span_count && spans[low].start <= entry.start {
                report(&mut aliases, &spans[low], entry);
                if aliases >= 24 {
                    break;
                }
            }
        }
    }
    // Two single-frame mappings on the same physical page.
    if aliases < 24 {
        for index in 1..page_count {
            if pages[index].start == pages[index - 1].start
                && pages[index].start != 0
            {
                report(&mut aliases, &pages[index - 1], &pages[index]);
                if aliases >= 24 {
                    break;
                }
            }
        }
    }
    if aliases == 0 {
        crate::console::write_str("ALIAS none\r\n");
    }
    if aliases >= 24 {
        crate::console::write_str("ALIAS ...more suppressed\r\n");
    }
    REPORTING.store(false, Ordering::Release);
}

/// The current task's TTBR0, for fault-dump diagnostics.
pub fn current_task_ttbr0() -> usize {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.ttbr0)
        .unwrap_or(0)
}

pub fn current_task_arguments() -> Vec<u8> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.arguments.clone())
        .unwrap_or_default()
}

pub fn current_task_cwd() -> String {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"))
}

pub fn cwd_in_use(path: &str) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && (task.cwd == path
                        || (task.cwd.starts_with(path)
                            && task.cwd.as_bytes().get(path.len()) == Some(&b'/')))
            })
        })
        .unwrap_or(false)
}

pub fn set_current_task_cwd(cwd: String) -> bool {
    if !cwd.starts_with('/') {
        return false;
    }
    if let Some(task) = current_task_mut() {
        task.cwd = cwd;
        return true;
    }
    false
}

/// Enforces the per-task read subtree. Native tasks have no prefix and stay
/// unrestricted; Linux tasks default to `/sfs` (the SFS mount).
pub fn read_path_allowed(task: u64, path: &str) -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(task) = manager.tasks.iter().find(|entry| entry.id == task) else {
        return false;
    };
    match &task.read_prefix {
        Some(prefix) => {
            if silver_microkernel::path_is_under(path, prefix) {
                return true;
            }
            // The writable scratch tree is also readable: toolkits keep
            // their runtime sockets and lock files there.
            #[cfg(feature = "linux-abi")]
            {
                // `/etc` is seeded read-only at boot (passwd, group,
                // nsswitch.conf); dbus-daemon needs it for its uid lookup.
                task.personality == Personality::Linux
                    && (silver_microkernel::path_is_under(path, LINUX_SCRATCH_PREFIX)
                        || silver_microkernel::path_is_under(path, "/etc"))
            }
            #[cfg(not(feature = "linux-abi"))]
            {
                false
            }
        }
        None => true,
    }
}

/// Writable tree for Linux tasks. `/tmp` lives on the root RamFs, so it is a
/// real, in-memory writable filesystem; SFS stays read-only **except** for the
/// service subtrees a real init writes (`/run`, `/var`, `/etc`, `/tmp`).
pub const LINUX_SCRATCH_PREFIX: &str = "/tmp";

/// VFS paths a Linux task may write. SFS itself remains read-only for
/// ordinary files; these are the subtrees OpenRC and its services need.
pub const LINUX_WRITABLE_PREFIXES: [&str; 5] =
    ["/tmp", "/sfs/run", "/sfs/var", "/sfs/etc", "/sfs/tmp"];

/// Whether `path` is under a writable subtree for Linux tasks.
pub fn linux_write_allowed(path: &str) -> bool {
    LINUX_WRITABLE_PREFIXES
        .iter()
        .any(|prefix| silver_microkernel::path_is_under(path, prefix))
}

/// RAM-backed writable subtrees (`/tmp`, `/run`, `/var`). Unlike SFS, these
/// report a non-executable `0644`: SFS stores no mode bits and every image
/// file must stay executable for shell/service scripts.
pub fn linux_ram_path(path: &str) -> bool {
    ["/tmp", "/sfs/run", "/sfs/var"]
        .iter()
        .any(|prefix| silver_microkernel::path_is_under(path, prefix))
}

pub fn current_read_path_allowed(path: &str) -> bool {
    read_path_allowed(current_task_id(), path)
}

pub fn task_infos() -> Vec<TaskInfo> {
    scheduler_manager()
        .map(|manager| {
            manager
                .tasks
                .iter()
                .filter(|task| task.id == IDLE_TASK_ID || !task.is_idle)
                .map(|task| TaskInfo {
                    id: task.id,
                    state: state_code(task.state),
                    program: task.program,
                    argument: task.argument,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn state_code(state: TaskState) -> u64 {
    match state {
        TaskState::Ready => 1,
        TaskState::Running => 2,
        TaskState::Sleeping(_) => 3,
        TaskState::Blocked => 4,
        TaskState::BlockedUntil(_) => 5,
        TaskState::Dead => 6,
        TaskState::Stopped => 7,
        // Zombies are kernel bookkeeping for `wait`: existing `SYS_TASK_LIST`
        // clients treat code 6 as "not selectable", so expose them as dead.
        TaskState::Zombie => 6,
    }
}

pub fn change_program_break(requested: usize) -> Option<usize> {
    with_group_owner(|task| task.change_program_break(requested))?
}

/// Allocates `length` bytes of anonymous memory for the current task and maps
/// them into a kernel-chosen free virtual range with `prot` permissions.
pub fn mmap_anonymous(length: usize, prot: u64) -> Option<usize> {
    with_group_owner(|task| task.mmap_anonymous(length, prot))?
}

/// Anonymous `MAP_FIXED`: place the mapping at an exact virtual address,
/// replacing any existing mapping in the range.
pub fn mmap_anonymous_at(length: usize, prot: u64, address: usize) -> Option<usize> {
    with_group_owner(|task| task.mmap_region(length, prot, Some(address), None, None, false))?
}

/// Lazily-backed anonymous mapping for the Linux personality: the range is
/// reserved now and each page is allocated on first access.
#[cfg(feature = "linux-abi")]
pub fn mmap_anonymous_lazy(length: usize, prot: u64) -> Option<usize> {
    with_group_owner(|task| task.mmap_region(length, prot, None, None, None, true))?
}

/// `MAP_FIXED` form of [`mmap_anonymous_lazy`].
#[cfg(feature = "linux-abi")]
pub fn mmap_anonymous_at_lazy(
    length: usize,
    prot: u64,
    address: usize,
) -> Option<usize> {
    with_group_owner(|task| task.mmap_region(length, prot, Some(address), None, None, true))?
}

/// Maps a file-backed region for the current task. `fixed` implements Linux
/// `MAP_FIXED`: any existing mapping in the range is replaced. `contents` is
/// copied into the fresh frames (zero-filled after its end).
pub fn mmap_file(
    length: usize,
    prot: u64,
    fixed: Option<usize>,
    contents: &[u8],
) -> Option<usize> {
    with_group_owner(|task| task.mmap_file(length, prot, fixed, contents))?
}

/// Maps a file-backed region one frame per page, filling each page through
/// `fill(offset, buffer)`. This is the path a dynamic loader uses for large
/// shared objects: it does not need a contiguous run and never reads the whole
/// file into the kernel heap.
pub fn mmap_file_paged<F>(
    length: usize,
    prot: u64,
    fixed: Option<usize>,
    fill: F,
) -> Option<usize>
where
    F: FnMut(usize, &mut [u8]) -> bool,
{
    with_group_owner(|task| task.mmap_file_paged(length, prot, fixed, fill))?
}

/// Maps a file lazily: pages are read through the VFS on first touch.
#[cfg(feature = "linux-abi")]
pub fn mmap_file_lazy(
    length: usize,
    prot: u64,
    fixed: Option<usize>,
    path: alloc::string::String,
    file_offset: usize,
) -> Option<usize> {
    with_group_owner(|task| task.mmap_file_lazy(length, prot, fixed, path, file_offset))?
}

/// Maps a memfd `MAP_SHARED` window. The frames stay owned by the memfd, so
/// every process that maps the same file sees the same bytes.
#[cfg(feature = "linux-abi")]
pub fn mmap_shared_file(
    length: usize,
    prot: u64,
    fixed: Option<usize>,
    physical_start: usize,
    owner: crate::linux_fd::MappedFile,
) -> Option<usize> {
    with_group_owner(|task| task.mmap_shared_file(length, prot, fixed, physical_start, owner))?
}

/// Changes protections on an existing mapping of the current task.
pub fn mprotect(address: usize, length: usize, prot: u64) -> bool {
    with_group_owner(|task| task.mprotect_range(address, length, prot)).unwrap_or(false)
}

/// Returns the current task's kernel-provided signal restorer page, creating
/// it on first use.
#[cfg(feature = "linux-abi")]
pub fn signal_trampoline() -> Option<usize> {
    with_group_owner(|task| task.signal_trampoline())?
}

/// COW write-fault entry point. Returns `true` when the faulting page was
/// copied and remapped, so the exception trampoline can retry the instruction.
#[cfg(feature = "linux-abi")]
pub fn cow_fault(address: usize, esr: u64) -> bool {
    let result = with_group_owner(|task| task.cow_fault(address, esr));
    // Kill-only telemetry: ring wraps hide everything else, so record just
    // the faults the handler could not recover (coverage: 0 none, 1 non-lazy
    // region, 2 lazy region, 3 lazy region with a zero prot).
    if result.is_none() || !result.unwrap_or(false) {
        if let Some(task) = scheduler_manager().and_then(|manager| {
            manager.tasks.iter().find(|task| task.id == current_tgid())
        }) {
            let page = address & !(PAGE_SIZE - 1);
            let mut coverage = 0_u64;
            let mut prot_seen = 0_u64;
            for region in &task.mmap_regions {
                let start = region.virtual_address;
                let end = start + region.page_count * PAGE_SIZE;
                if page >= start && page < end {
                    coverage = if region.lazy { 2 } else { 1 };
                    prot_seen = region.prot;
                    break;
                }
            }
            if coverage == 2 && prot_seen == 0 {
                coverage = 3;
            }
            crate::scheduler::region_cover_record(
                address,
                coverage,
                task.mmap_regions.len() as u64,
                esr,
            );
        }
    }
    result.unwrap_or(false)
}

/// Region coverage of one address in the thread-group owner's mmap list.
/// Returns (coverage, region prot): 0 none, 1 non-lazy, 2 lazy, 3 lazy with
/// a zero prot (an mprotect that never reached the reservation).
#[cfg(feature = "linux-abi")]
pub fn fault_coverage(address: usize) -> (u64, u64) {
    let page = address & !(PAGE_SIZE - 1);
    let Some(task) = scheduler_manager().and_then(|manager| {
        manager.tasks.iter().find(|task| task.id == current_tgid())
    }) else {
        return (0, 0);
    };
    for region in &task.mmap_regions {
        let start = region.virtual_address;
        let end = start + region.page_count * PAGE_SIZE;
        if page >= start && page < end {
            if region.lazy {
                return (if region.prot == 0 { 3 } else { 2 }, region.prot);
            }
            return (1, region.prot);
        }
    }
    (0, 0)
}

pub const REGION_COVER_LEN: usize = 32;
pub static REGION_COVER_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut REGION_COVER: [[u64; 6]; REGION_COVER_LEN] = [[0; 6]; REGION_COVER_LEN];

#[cfg(feature = "linux-abi")]
pub fn region_cover_record(address: usize, coverage: u64, regions: u64, esr: u64) {
    let index = REGION_COVER_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let task = current_task_id();
    unsafe {
        let ring = core::ptr::addr_of_mut!(REGION_COVER);
        (*ring)[index % REGION_COVER_LEN] = [address as u64, coverage, regions, task, esr, 0];
    }
}

/// Last COW-path decision, for the fault dump (console writes from exception
/// context drop under contention; this buffer is read back by
/// [`cow_diagnostic`] instead). Fault context holds the kernel lock, so a
/// plain static is single-writer.
static COW_REASON: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static mut COW_REASON_TEXT: [u8; 128] = [0_u8; 128];

#[cfg(feature = "linux-abi")]
fn cow_reason_set(text: &[u8]) {
    let capacity = unsafe { (*core::ptr::addr_of!(COW_REASON_TEXT)).len() };
    let take = text.len().min(capacity);
    unsafe {
        let base = core::ptr::addr_of_mut!(COW_REASON_TEXT) as *mut u8;
        core::ptr::copy_nonoverlapping(text.as_ptr(), base, take);
    }
    COW_REASON.store(take, core::sync::atomic::Ordering::Release);
}

#[cfg(feature = "linux-abi")]
fn cow_reason_get(buffer: &mut [u8]) -> usize {
    let capacity = unsafe { (*core::ptr::addr_of!(COW_REASON_TEXT)).len() };
    let take = COW_REASON.load(core::sync::atomic::Ordering::Acquire);
    let take = take.min(buffer.len()).min(capacity);
    unsafe {
        let base = core::ptr::addr_of!(COW_REASON_TEXT) as *const u8;
        buffer[..take].copy_from_slice(core::slice::from_raw_parts(base, take));
    }
    take
}

/// Every mmap/munmap the Linux ABI performs (address, length, prot|flags
/// tag, task) so a monitor dump shows what bookkeeping produced a VA.
pub const MMAP_RING_LEN: usize = 64;
pub static MMAP_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut MMAP_RING: [[u64; 5]; MMAP_RING_LEN] = [[0; 5]; MMAP_RING_LEN];

#[cfg(feature = "linux-abi")]
pub fn mmap_ring_record(kind: u64, address: usize, length: usize, prot: u64) {
    let index = MMAP_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let task = current_task_id();
    unsafe {
        let ring = core::ptr::addr_of_mut!(MMAP_RING);
        (*ring)[index % MMAP_RING_LEN] =
            [kind, address as u64, length as u64, prot as u64, task];
    }
}

/// Successful lazy backings (page, prot, physical, task) - paired with
/// LAZY_FAIL to tell 'map does not stick' from 'map never runs'.
pub const LAZY_OK_LEN: usize = 64;
pub static LAZY_OK_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut LAZY_OK: [[u64; 6]; LAZY_OK_LEN] = [[0; 6]; LAZY_OK_LEN];

/// Lazy-backing failures, read through the QEMU monitor dump the same way
/// the fault ring is. 8 words per record: page, reason tag (4 bytes), mem
/// frames, mmap bytes, region count, task.
pub const LAZY_FAIL_LEN: usize = 32;
pub static LAZY_FAIL_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut LAZY_FAIL: [[u64; 8]; LAZY_FAIL_LEN] = [[0; 8]; LAZY_FAIL_LEN];

#[cfg(feature = "linux-abi")]
pub fn lazy_fail_record(page: usize, reason: &[u8], mem_frames: usize, mmap_bytes: usize, regions: usize) {
    let index = LAZY_FAIL_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut tag = [0_u8; 8];
    let take = reason.len().min(8);
    tag[..take].copy_from_slice(&reason[..take]);
    let tag_word = u64::from_le_bytes(tag);
    let task = current_task_id();
    unsafe {
        let ring = core::ptr::addr_of_mut!(LAZY_FAIL);
        (*ring)[index % LAZY_FAIL_LEN] = [
            page as u64,
            tag_word,
            mem_frames as u64,
            mmap_bytes as u64,
            regions as u64,
            task,
            0,
            0,
        ];
    }
}

/// Diagnostic for the fault dump: which task the COW path would have acted
/// on (the thread-group owner, not necessarily the faulting CPU's task), and
/// which image run covers the address with which page kind. Returns the
/// formatted length inside `buffer`.
#[cfg(feature = "linux-abi")]
pub fn cow_diagnostic(address: usize, buffer: &mut [u8]) -> usize {
    let mut length = 0_usize;
    fn push(buffer: &mut [u8], length: &mut usize, text: &[u8]) {
        let room = buffer.len().saturating_sub(*length);
        let take = text.len().min(room);
        buffer[*length..*length + take].copy_from_slice(&text[..take]);
        *length += take;
    }
    fn push_num(buffer: &mut [u8], length: &mut usize, mut value: u64) {
        let mut digits = [0_u8; 20];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for index in (0..count).rev() {
            push(buffer, length, &[digits[index]]);
        }
    }
    push(buffer, &mut length, b"COWDIAG task=");
    push_num(buffer, &mut length, current_task_id());
    push(buffer, &mut length, b" tgid=");
    push_num(buffer, &mut length, current_tgid());
    let owner = current_tgid();
    let covered = scheduler_manager().and_then(|manager| {
        manager.tasks.iter().find(|task| task.id == owner)
    }).and_then(|task| {
        let image = task.linux_image.as_ref()?;
        let page = address & !(PAGE_SIZE - 1);
        image.runs.iter().find_map(|run| {
            if page >= run.virtual_address
                && page < run.virtual_address + run.page_count * PAGE_SIZE
            {
                let index = (page - run.virtual_address) / PAGE_SIZE;
                Some((
                    run.virtual_address as u64,
                    run.page_count as u64,
                    run.kinds[index] as u64,
                    run.physical_start as u64,
                    index as u64,
                ))
            } else {
                None
            }
        })
    });
    match covered {
        Some((va, pages, kind, pa, index)) => {
            push(buffer, &mut length, b" run va=");
            push_num(buffer, &mut length, va);
            push(buffer, &mut length, b" pages=");
            push_num(buffer, &mut length, pages);
            push(buffer, &mut length, b" idx=");
            push_num(buffer, &mut length, index);
            push(buffer, &mut length, b" kind=");
            push_num(buffer, &mut length, kind);
            push(buffer, &mut length, b" pa=");
            push_num(buffer, &mut length, pa);
        }
        None => {
            push(buffer, &mut length, b" no-covering-run-in-owner");
        }
    }
    push(buffer, &mut length, b" frames used=");
    push_num(buffer, &mut length, frame_allocator::used_pages() as u64);
    push(buffer, &mut length, b" free=");
    push_num(buffer, &mut length, frame_allocator::free_pages() as u64);
    let reason_len = cow_reason_get(&mut buffer[length..]);
    length += reason_len;
    length
}

/// Allocates a fresh run and copies an old shared run into it.
#[cfg(feature = "linux-abi")]
fn cow_copy_run(
    old_physical: usize,
    page_count: usize,
    old_generation: u32,
) -> Option<(usize, u32)> {
    let (physical, generation) =
        frame_allocator::alloc_user_contiguous_pages_tagged(page_count)?;
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(old_physical) as *const u8,
            phys_to_virt(physical) as *mut u8,
            page_count * PAGE_SIZE,
        );
    }
    let _ = old_generation;
    Some((physical, generation))
}

/// Unmaps an anonymous region previously returned by `mmap_anonymous`.
pub fn munmap(address: usize, length: usize) -> bool {
    with_group_owner(|task| task.munmap(address, length)).unwrap_or(false)
}

pub fn start() -> ! {
    let saved_daif = interrupts::save_and_disable_irq();
    let manager = scheduler_manager().unwrap();
    let current_index = smp::current_index();
    let Some(first_task) = manager.tasks.get_mut(current_index) else {
        loop {
            interrupts::wait_for_irq();
        }
    };

    // The boot switch runs under the kernel lock; `silver_user_bootstrap`
    // drops it again right before `eret`-ing into the first user task, so
    // the acquire/release pair stays balanced.
    smp::bkl_acquire();
    first_task.state = TaskState::Running;
    first_task.running_on = Some(0);
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    SCHEDULER_STARTED.store(true, Ordering::Release);
    #[cfg(feature = "smp")]
    crate::watchdog::cpu_started(0);
    trace_switch(
        0,
        first_task.id,
        first_task.context.stack_pointer,
        first_task.context.elr,
    );
    let first_context = &first_task.context as *const TaskContext;
    let first_fp = first_task.fp;
    let boot_context = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) };

    // Install the first task's vector register file; `silver_user_bootstrap`
    // restores it immediately before entering EL0.
    unsafe {
        *smp::this_cpu().fp.get() = first_fp;
    }

    mmu::switch_address_space(first_task.ttbr0);

    unsafe {
        silver_context_switch(boot_context, first_context);
    }

    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

/// One scheduling pass for a task that already holds the kernel lock:
/// releases it (a blocked task must not pin the lock — another CPU has to be
/// able to wake tasks), runs the pass, and re-acquires before returning so
/// the caller keeps executing with the lock held.
pub fn schedule_released() {
    smp::bkl_release();
    schedule();
    smp::bkl_acquire();
}

pub fn schedule() {
    if !SCHEDULER_READY.load(Ordering::Acquire) {
        // Boot: no valid current task yet (see `SCHEDULER_READY`).
        return;
    }
    let Some(manager) = scheduler_manager() else {
        return;
    };
    if manager.tasks.is_empty() {
        return;
    }

    #[cfg(feature = "smp")]
    if smp::this_cpu_id() != 0
        && !smp::AP_TASK_SCHEDULING.load(core::sync::atomic::Ordering::Acquire)
    {
        // Phase 1: APs stay parked in `wait_for_irq` after GIC/timer bring-up.
        return;
    }

    // A pass is not re-entrant on one CPU. The snapshot/remap pair below and
    // the `running_on = None` -> publish-new-index transition are only
    // consistent once the pass completes; a timer IRQ that ran a second pass
    // in between would leave the outer pass publishing a stale index when it
    // resumes. Mask IRQs for the whole pass and refuse a nested entry (the
    // churn harness re-enables IRQs on purpose while it waits, so the mask
    // alone is not a complete guard). The mask is dropped again only after
    // the kernel lock has been released, right before returning or resuming.
    let pass_cpu = smp::this_cpu_id();
    #[cfg(feature = "smp")]
    crate::watchdog::note_progress(pass_cpu);
    if SCHED_PASS_ACTIVE[pass_cpu].swap(true, Ordering::AcqRel) {
        return;
    }
    let saved_daif = interrupts::save_and_disable_irq();

    // The kernel lock is held for the whole pass. If this call switches
    // away, the outgoing task is suspended before the matching release and
    // the incoming task resumes inside its own suspended `schedule`, so the
    // CPU keeps the lock across the switch without a transfer protocol.
    crate::smp::probe(5);
    smp::bkl_acquire();
    {
        static SCHED_SEEN: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !SCHED_SEEN.swap(true, core::sync::atomic::Ordering::AcqRel) {
            crate::console::write_str("SMP: schedule() first entry\r\n");
        }
    }

    let live_task_id = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.id)
        .unwrap_or(IDLE_TASK_ID);
    let live_is_dead = manager
        .tasks
        .get(smp::current_index())
        .map(|task| matches!(task.state, TaskState::Dead | TaskState::Zombie))
        .unwrap_or(false);

    reap_dead_tasks(manager);
    // The reap path removes tasks (shifting indices) and remaps every CPU's
    // current index. Verify the state machine immediately afterwards: this is
    // the riskiest mutation in the scheduler, and `smp-selftest` turns a silent
    // `running_on`/`current_index` drift into a loud boot failure instead of a
    // task resuming with another task's address space.
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    if SCHEDULER_STARTED.load(Ordering::Acquire) {
        if let Err(reason) = verify_scheduling_invariants(manager) {
            console::write_str("SMP SELFTEST: sched invariant FAIL (");
            console::write_str(reason);
            console::write_str(")\r\n");
            panic!("scheduling invariant violated");
        }
    }
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    report_ready_queues_once();
    wake_expired_tasks(manager, timer::ticks());

    if AP_BOOT_PENDING.swap(false, core::sync::atomic::Ordering::AcqRel) {
        #[cfg(feature = "smp")]
        smp::boot_secondary_cpus();
    }

    let current_index = smp::current_index();
    let had_current = current_index < manager.tasks.len();
    // Boot window: `current_index` may already name idle while the CPU is
    // still on the boot stack (a timer IRQ during `scheduler::init`). Saving
    // that SP into idle's context corrupts it. `start()` performs the real
    // first switch; until then `schedule` must not switch at all. An AP's
    // first pass has no current task (`current_index == usize::MAX`) and is
    // handled by the dummy-context path below.
    if !live_is_dead && had_current {
        let sp_now: usize;
        unsafe {
            core::arch::asm!("mov {}, sp", out(reg) sp_now, options(nostack));
        }
        let current = &manager.tasks[current_index];
        let physical = sp_now.wrapping_sub(crate::memory::KERNEL_VIRT_BASE);
        if !current.stack.contains(physical.saturating_sub(1)) {
            SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
            smp::bkl_release();
            interrupts::restore_irq(saved_daif);
            return;
        }
    }
    if had_current && manager.tasks[current_index].state == TaskState::Running {
        manager.tasks[current_index].state = TaskState::Ready;
    }

    let scan_base = if had_current { current_index } else { 0 };
    let Some(next_index) = find_next_task(manager, smp::this_cpu_id(), scan_base) else {
        if had_current && manager.tasks[current_index].state == TaskState::Ready {
            manager.tasks[current_index].state = TaskState::Running;
        }
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    };
    // A runnable task must not be punished with a full idle round trip.
    // `find_next_task` only sees queued tasks, and the outgoing task is enqueued
    // *after* this decision, so when it is Ready on this CPU (it yielded, or a
    // tick preempted it) and the only candidate is an idle task, the "switch"
    // would park the CPU in `wfi` until the next tick. That capped a `yield`
    // loop at the tick rate: `bench sched` measured ~200 yields/s (5 ms each,
    // half of the 10 ms tick) instead of a same-CPU resumption. Idle still runs
    // whenever nothing is runnable, i.e. when the outgoing task is not Ready.
    if had_current
        && !live_is_dead
        && manager.tasks[next_index].is_idle
        && manager.tasks[current_index].state == TaskState::Ready
        && !manager.tasks[current_index].is_idle
    {
        manager.tasks[current_index].state = TaskState::Running;
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    }

    if had_current && next_index == current_index && !live_is_dead {
        // Self-continuation: this CPU keeps running the same task and keeps
        // holding the kernel lock for it. The probe above marked it Ready so
        // `find_next_task` could consider it; restore Running before another
        // CPU can mistake it for runnable.
        manager.tasks[current_index].state = TaskState::Running;
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    }

    // Switching away: the outgoing task stops running on this CPU. An AP's
    // first pass has no outgoing task, so there is no state to clear.
    if had_current {
        let outgoing_ready = match manager.tasks.get_mut(current_index) {
            Some(outgoing) => {
                outgoing.running_on = None;
                // The outgoing task may be `Ready` (it yielded, or a wake moved
                // it back to Ready while it still owned this CPU). Now that no
                // CPU owns it, put it on this CPU's ready queue.
                outgoing.state == TaskState::Ready
            }
            None => {
                console::write_str("SCHEDULE: current_index out of range: ");
                console::write_dec_u64(current_index as u64);
                console::write_str("\r\n");
                false
            }
        };
        if outgoing_ready {
            enqueue_ready(manager, smp::this_cpu_id(), current_index);
        }
    }

    // From here to the context switch the code only uses the raw `tasks`
    // pointer: `next_task` used to be a `&mut Task` minted from the vector
    // while `manager` kept being reborrowed (`tasks.len()`), which invalidates
    // the reference under Stacked Borrows — exactly the class of UB the
    // optimizer turned into a bare `b .` self-loop elsewhere.
    let tasks_len = manager.tasks.len();
    let tasks = manager.tasks.as_mut_ptr();
    let next_task = unsafe { &mut *tasks.add(next_index) };
    let saved_stack_pointer = next_task.context.stack_pointer as usize;
    if !next_task
        .stack
        .contains(virt_to_phys(saved_stack_pointer) - 1)
    {
        console::write_str("BAD SP task=");
        console::write_dec_u64(next_task.id);
        console::write_str(" sp=");
        console::write_hex_u64(next_task.context.stack_pointer);
        console::write_str(" stack=[");
        console::write_hex_u64(next_task.stack.start as u64);
        console::write_str(",");
        console::write_hex_u64(next_task.stack.end as u64);
        console::write_str("] cur_idx=");
        console::write_dec_u64(current_index as u64);
        console::write_str(" live_id=");
        console::write_dec_u64(live_task_id);
        console::write_str(" dead=");
        console::write_dec_u64(u64::from(live_is_dead));
        console::write_str(" next_idx=");
        console::write_dec_u64(next_index as u64);
        console::write_str("\r\n");
        dump_switch_trace();
        panic!("bad saved stack pointer");
    }

    let next_id = next_task.id;
    let next_ttbr0 = next_task.ttbr0;
    let next_context_stack_pointer = next_task.context.stack_pointer;
    let next_context_elr = next_task.context.elr;
    next_task.state = TaskState::Running;
    next_task.running_on = Some(smp::this_cpu_id());
    smp::set_current_task(next_index, next_id);

    let has_current = current_index < tasks_len;
    let current_context = if live_is_dead {
        unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) }
    } else if has_current {
        unsafe { &mut (*tasks.add(current_index)).context }
    } else {
        // An AP's first pass has no current task: its outgoing registers are
        // parked in a dummy context that is never restored (the AP's boot
        // stack frame is abandoned once it switches into a real task).
        unsafe { &mut AP_DUMMY_CONTEXTS[smp::this_cpu_id()] }
    };
    let from_id = if live_is_dead || !has_current {
        live_task_id
    } else {
        unsafe { (*tasks.add(current_index)).id }
    };
    // An AP before its first pick has no current task and no current TTBR0:
    // force the address-space switch to the first task it runs.
    let current_ttbr0 = if has_current {
        unsafe { (*tasks.add(current_index)).ttbr0 }
    } else {
        0
    };

    if live_is_dead || next_ttbr0 != current_ttbr0 {
        mmu::switch_address_space(next_ttbr0);
    }

    trace_switch(
        from_id,
        next_id,
        next_context_stack_pointer,
        next_context_elr,
    );

    // FP/SIMD file: the outgoing task's live copy lives in `PerCpu::fp`
    // (its last EL0 entry saved it there). Park it in the task record and
    // install the incoming task's file on this CPU; the incoming task's
    // exception epilogue restores the vector registers before `eret`.
    let cpu = smp::this_cpu();
    unsafe {
        if has_current && !live_is_dead {
            (*tasks.add(current_index)).fp = *cpu.fp.get();
        }
        *cpu.fp.get() = (*tasks.add(next_index)).fp;
    }

    // The pass is complete: the new `current_index` is published, so a later
    // pass (including a preemption right after the resume) starts from a
    // consistent cut. IRQs stay masked across the switch itself; the incoming
    // task re-enables them from its own saved state.
    SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
    unsafe {
        silver_context_switch(current_context, &(*tasks.add(next_index)).context);
    }

    // Resumed here as the incoming task, inside our own suspended `schedule`:
    // complete the release of this pass's acquire (see the locking contract
    // in `smp::bkl_acquire`). A task that never ran before enters through
    // `silver_user_bootstrap` instead, whose `bkl_release_asm` is the matching
    // release for a first dispatch; this line only executes on the resumed
    // path, so the kernel lock never stays pinned while a task sleeps.
    smp::bkl_release();
    interrupts::restore_irq(saved_daif);
}

/// Result of a successful `execve`: the EL0 entry point and stack pointer the
/// syscall trampoline must resume at.
/// Writes the child TID into the child's copy of `address` after a plain
/// `fork`. glibc's `_Fork` passes `CLONE_CHILD_SETTID`; without this the
/// child TCB keeps the parent's/garbage TID and the first libc call after
/// fork faults. The page is broken out of COW in the child first, then the
/// write is performed with the child's TTBR0 installed.
#[cfg(feature = "linux-abi")]
pub fn set_child_tid(child_id: u64, address: u64, value: u32, clear_on_exit: bool) -> bool {
    let (child_ttbr0, parent_ttbr0) = {
        let manager = match scheduler_manager() {
            Some(manager) => manager,
            None => return false,
        };
        let Some(child) = manager.tasks.iter().find(|task| task.id == child_id) else {
            return false;
        };
        if child.ttbr0 == 0 || child.running_on.is_some() {
            return false;
        }
        let parent_ttbr0 = manager
            .tasks
            .get(smp::current_index())
            .map(|task| task.ttbr0)
            .unwrap_or(0);
        (child.ttbr0, parent_ttbr0)
    };
    // The TCB lives in a private page inherited read-only; break it in the
    // child. A page that is already writable returns false here and is
    // handled by the validation below.
    if let Some(child) = scheduler_manager()
        .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == child_id))
    {
        let _ = child.cow_fault(address as usize, 1 << 6);
    }
    mmu::switch_address_space(child_ttbr0);
    let writable = mmu::valid_user_slice_mut(address, 4);
    let written = writable
        && crate::user_memory::copy_to_user(&value.to_le_bytes(), address).is_ok();
    mmu::switch_address_space(parent_ttbr0);
    if !written {
        return false;
    }
    if clear_on_exit {
        if let Some(child) = scheduler_manager()
            .and_then(|manager| manager.tasks.iter_mut().find(|task| task.id == child_id))
        {
            child.clear_child_tid = Some(address);
        }
    }
    true
}

#[cfg(feature = "linux-abi")]
pub struct ExecResult {
    pub entry: usize,
    pub stack_pointer: u64,
}

/// Replaces the current task's Linux image and address space (`execve`).
///
/// The kernel stack, task id, capabilities, cwd and fd table are preserved;
/// the executable image, heap, anonymous mappings, user stack and page tables
/// are replaced. The new space is built and populated while the old one is
/// still current, then the CPU switches over and the old resources are
/// released — `destroy_task_space` refuses a space that is still current, so
/// the switch must happen first.
#[cfg(feature = "linux-abi")]
pub fn exec_linux_current(
    image: LinuxImage,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<ExecResult> {
    let auxv = linux_auxv(AuxvImage::linux(&image));
    let mut layout = match silver_microkernel::build_linux_stack(
        &argv,
        &envp,
        &auxv,
        mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
        mmu::USER_STACK_SIZE - 1024,
    ) {
        Some(layout) => layout,
        None => return None,
    };
    // AT_RANDOM (the first 16 bytes) must be fresh per exec.
    crate::entropy::fill(&mut layout.block[..16]);
    let entry = image.entry;
    let image_frames = image.page_count();
    // The task keeps its kernel stack, fixed tables, private page and user
    // stack; only the image (and the heap/mmap tables, now empty) is charged.
    let ledger_frames = TASK_STACK_PAGES
        + mmu::SPACE_TABLE_FRAMES
        + 1
        + TASK_USER_STACK_PAGES
        + image_frames;
    if !mem_policy::task_charge_ok(0, ledger_frames, task_frame_limit()) {
        return None;
    }

    let (new_stack_start, new_stack_generation) =
        frame_allocator::alloc_user_contiguous_pages_tagged(TASK_USER_STACK_PAGES)?;
    let Some((new_private, new_private_generation)) = frame_allocator::alloc_user_page_tagged()
    else {
        frame_allocator::free_contiguous_pages_tagged(
            new_stack_start,
            TASK_USER_STACK_PAGES,
            new_stack_generation,
        );
        return None;
    };
    let Some((new_ttbr0, new_token)) =
        mmu::create_task_space(new_private, new_stack_start, None, false)
    else {
        frame_allocator::free_page_tagged(new_private, new_private_generation);
        frame_allocator::free_contiguous_pages_tagged(
            new_stack_start,
            TASK_USER_STACK_PAGES,
            new_stack_generation,
        );
        return None;
    };
    if !mmu::install_linux_image(new_ttbr0, &image) {
        mmu::destroy_task_space(
            new_token,
            new_private,
            new_private_generation,
            mmu::asid_of(new_ttbr0),
        );
        frame_allocator::free_contiguous_pages_tagged(
            new_stack_start,
            TASK_USER_STACK_PAGES,
            new_stack_generation,
        );
        return None;
    }

    // Publish the new initial stack before the task can run on it.
    let offset = mmu::USER_STACK_SIZE - layout.block.len();
    unsafe {
        core::ptr::copy_nonoverlapping(
            layout.block.as_ptr(),
            phys_to_virt(new_stack_start + offset) as *mut u8,
            layout.block.len(),
        );
    }

    let (old_resources, old_stack, old_stack_generation) = {
        let task = current_task_mut()?;
        let old_resources = SpaceResources {
            ttbr0: task.ttbr0,
            space_token: task.space_token,
            private_page: task.private_page,
            private_page_generation: task.private_page_generation,
            mmap_regions: core::mem::take(&mut task.mmap_regions),
            heap_pages: core::mem::take(&mut task.heap_pages),
            dma_pages: core::mem::take(&mut task.dma_pages),
            user_image: task.user_image.take(),
            linux_image: task.linux_image.take(),
        };
        let old_stack = task.user_stack;
        let old_stack_generation = task.user_stack_generation;

        task.ttbr0 = new_ttbr0;
        task.space_token = new_token;
        task.private_page = new_private;
        task.private_page_generation = new_private_generation;
        task.user_stack = MemoryRegion::new(
            new_stack_start,
            new_stack_start + TASK_USER_STACK_SIZE,
        );
        task.user_stack_generation = new_stack_generation;
        task.linux_image = Some(image);
        task.user_image = None;
        task.mmap_bytes = 0;
        task.signal_trampoline = None;
        task.program_break = mmu::USER_HEAP_VA;
        task.mem_frames = ledger_frames;
        task.resources_released = false;
        task.argv = argv;
        task.envp = envp;
        // exec starts a fresh program: no vector state may survive from the
        // superseded image. The next EL0 exit restores these zeroes.
        task.fp = smp::FpState::zero();
        unsafe { *smp::this_cpu().fp.get() = smp::FpState::zero(); }
        (old_resources, old_stack, old_stack_generation)
    };

    // TTBR0 must point at the new space before the old tables are retired.
    mmu::switch_address_space(new_ttbr0);
    release_space_resources(&old_resources, &[]);
    if !old_stack.is_empty() {
        frame_allocator::free_contiguous_pages_tagged(
            old_stack.start,
            TASK_USER_STACK_PAGES,
            old_stack_generation,
        );
    }

    Some(ExecResult {
        entry,
        stack_pointer: layout.sp as u64,
    })
}

/// Cleanup guard for a partially built fork. On success every field is taken
/// into the child task, so `Drop` sees empty state and frees nothing.
#[cfg(feature = "linux-abi")]
struct ForkParts {
    image: Option<LinuxImage>,
    /// `(space token, asid)` once the address space exists.
    space: Option<(TableToken, u32)>,
    /// The private page; owned by the space once `space` is `Some`.
    private: Option<(usize, u32)>,
    stack: Option<(usize, u32)>,
    heap: Vec<(usize, u32)>,
    mmap: Vec<MmapRegion>,
}

#[cfg(feature = "linux-abi")]
impl Drop for ForkParts {
    fn drop(&mut self) {
        if let Some((token, asid)) = self.space.take() {
            let (private, generation) = self.private.take().unwrap_or((0, 0));
            let _ = mmu::destroy_task_space(token, private, generation, asid as u16);
        } else if let Some((private, generation)) = self.private.take() {
            let _ = frame_allocator::free_page_tagged(private, generation);
        }
        if let Some((start, generation)) = self.stack.take() {
            let _ = frame_allocator::free_contiguous_pages_tagged(
                start,
                TASK_USER_STACK_PAGES,
                generation,
            );
        }
        for &(page, generation) in &self.heap {
            let _ = frame_allocator::free_page_tagged(page, generation);
        }
        for region in &self.mmap {
            if region.lazy {
                for &(page, generation) in region.pages.values() {
                    let _ = frame_allocator::free_page_tagged(page, generation);
                }
                continue;
            }
            if !region.frames.is_empty() {
                for &(page, generation) in &region.frames {
                    let _ = frame_allocator::free_page_tagged(page, generation);
                }
                continue;
            }
            let _ = frame_allocator::free_contiguous_pages_tagged(
                region.physical_start,
                region.page_count,
                region.generation,
            );
        }
        if let Some(image) = &self.image {
            image.release();
        }
    }
}

/// Forks the current Linux task (`clone` with `SIGCHLD`, no shared VM).
///
/// The child shares the parent's image, heap, anonymous mappings, private
/// page and user stack copy-on-write: frames gain a refcount, writable pages
/// are mapped read-only in both spaces, and the first write copies the
/// containing run through `cow_fault`. The kernel stack is new and starts with
/// a copy of the parent's SVC frame whose `x0` is zero, so the child returns
/// from `clone` as a new process. File descriptors and the Linux fd table are
/// duplicated, and the child's parent is the caller.
#[cfg(feature = "linux-abi")]
pub fn fork_current(frame: &[u64; crate::syscall::FRAME_QUADWORDS]) -> Option<u64> {
    fork_current_with_sp(frame, None)
}

/// Fork the current Linux task. `user_sp` overrides the child's initial
/// userspace stack: `clone(CLONE_VM|CLONE_VFORK)` from musl's `__clone`
/// (used by `posix_spawn`) hands the child a dedicated stack and a function
/// pointer stored at its top; resuming on the parent's stack would make the
/// child read garbage and jump to it.
#[cfg(feature = "linux-abi")]
pub fn fork_current_with_sp(
    frame: &[u64; crate::syscall::FRAME_QUADWORDS],
    user_sp: Option<u64>,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    // A plain `fork` may be called from a thread. The child is a single-thread
    // process whose address space is a copy of the *process* (the group
    // leader), not of the calling thread, and the leader owns the image,
    // heap and mmap regions. The resume frame and TLS still come from the
    // caller.
    let parent_id = current_tgid();
    let parent_index = manager
        .tasks
        .iter()
        .position(|task| task.id == parent_id)?;

    // Snapshot everything needed from the parent before mutating the table.
    let (
        parent_image,
        parent_stack,
        parent_stack_generation,
        parent_private,
        parent_private_generation,
        parent_heap,
        parent_mmap,
        parent_cwd,
        parent_argv,
        parent_envp,
        parent_arguments,
        parent_break,
        parent_mmap_bytes,
        parent_trampoline,
        parent_prefix,
        parent_read_prefix,
        parent_tls,
        parent_quota,
        parent_priority,
        parent_fp,
    ) = {
        let parent = manager.tasks.get(parent_index)?;
        if parent.personality != Personality::Linux || parent.user_image.is_some() { return None;
        }
        (
            match parent.linux_image.as_ref()?.share() {
                Some(image) => image,
                None => { return None;
                }
            },
            parent.user_stack,
            parent.user_stack_generation,
            parent.private_page,
            parent.private_page_generation,
            parent.heap_pages.clone(),
            parent.mmap_regions.clone(),
            parent.cwd.clone(),
            parent.argv.clone(),
            parent.envp.clone(),
            parent.arguments.clone(),
            parent.program_break,
            parent.mmap_bytes,
            parent.signal_trampoline,
            parent.fs_write_prefix.clone(),
            parent.read_prefix.clone(),
            current_user_tls(),
            parent.fs_quota,
            parent.priority,
            // The caller is inside its own syscall, so the live file is the
            // parent's state at the `clone` call, not the older copy parked
            // in the task record.
            unsafe { *smp::this_cpu().fp.get() },
        )
    };
    if parent_stack.is_empty() { return None;
    }

    let mut parts = ForkParts {
        image: Some(parent_image),
        space: None,
        private: None,
        stack: None,
        heap: Vec::new(),
        mmap: Vec::new(),
    };

    // Copy-on-write: share the parent's pages instead of allocating copies.
    // `ForkParts::drop` decrements these refs if a later step fails.
    if !frame_allocator::share_contiguous_pages_tagged(
        parent_stack.start,
        TASK_USER_STACK_PAGES,
        parent_stack_generation,
    ) { return None;
    }
    parts.stack = Some((parent_stack.start, parent_stack_generation));
    if !frame_allocator::share_page_tagged(parent_private, parent_private_generation) { return None;
    }
    parts.private = Some((parent_private, parent_private_generation));

    let mut heap = Vec::with_capacity(parent_heap.len());
    for &(page, generation) in &parent_heap {
        if !frame_allocator::share_page_tagged(page, generation) { return None;
        }
        heap.push((page, generation));
    }
    parts.heap = heap;
    for region in &parent_mmap {
        if region.shared.is_some() {
            // A MAP_SHARED window is not copied: the child maps the same
            // memfd frames writable, so both sides see each other's writes.
            parts.mmap.push(region.clone());
            continue;
        }
        if region.lazy {
            // Copy the pages the parent has already touched. They hold live
            // data (a pthread TCB and stack, glibc heaps), so handing the
            // child a zeroed reservation would make its first libc call
            // dereference a null TCB field. Untouched pages stay lazy.
            let mut pages: BTreeMap<u32, (usize, u32)> = BTreeMap::new();
            for (&offset, &(page, _generation)) in &region.pages {
                let Some((new_page, new_generation)) =
                    frame_allocator::alloc_user_page_tagged()
                else {
                    for (page, generation) in pages.values() {
                        let _ = frame_allocator::free_page_tagged(*page, *generation);
                    }
                    return None;
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        phys_to_virt(page) as *const u8,
                        phys_to_virt(new_page) as *mut u8,
                        PAGE_SIZE,
                    );
                }
                pages.insert(offset, (new_page, new_generation));
            }
            parts.mmap.push(MmapRegion {
                virtual_address: region.virtual_address,
                page_count: region.page_count,
                physical_start: 0,
                generation: 0,
                prot: region.prot,
                shared: SharedMapping::default(),
                frames: Vec::new(),
                lazy: true,
                pages,
                file: region.file.clone(),
            });
            continue;
        }
        if !region.frames.is_empty() {
            // File-backed pages are not one contiguous run. Share the frames
            // copy-on-write instead of copying them: a Phosh process maps
            // ~120 MiB of libraries, and an eager copy made `fork` (which
            // glycin uses to spawn its image loader) exhaust DRAM and fail
            // with EAGAIN. Both sides are write-protected below, so the first
            // write on either side copies a single page.
            let mut shared: Vec<(usize, u32)> = Vec::new();
            for &(page, generation) in &region.frames {
                if !frame_allocator::share_page_tagged(page, generation) {
                    for (page, generation) in shared {
                        let _ = frame_allocator::free_page_tagged(page, generation);
                    }
                    return None;
                }
                shared.push((page, generation));
            }
            parts.mmap.push(MmapRegion {
                virtual_address: region.virtual_address,
                page_count: region.page_count,
                physical_start: 0,
                generation: 0,
                prot: region.prot,
                shared: SharedMapping::default(),
                frames: shared,
                lazy: false,
                pages: BTreeMap::new(),
                file: None,
            });
            continue;
        }
        if !frame_allocator::share_contiguous_pages_tagged(
            region.physical_start,
            region.page_count,
            region.generation,
        ) { return None;
        }
        parts.mmap.push(MmapRegion {
            virtual_address: region.virtual_address,
            page_count: region.page_count,
            physical_start: region.physical_start,
            generation: region.generation,
            prot: region.prot,
            shared: SharedMapping::default(),
            frames: Vec::new(),
            lazy: false,
            pages: BTreeMap::new(),
            file: None,
        });
    }

    let heap_frames = parts.heap.len();
    let mmap_frames: usize = parts
        .mmap
        .iter()
        .map(|region| {
            if region.lazy {
                region.pages.len()
            } else {
                region.page_count
            }
        })
        .sum();
    let ledger_frames = TASK_STACK_PAGES
        + mmu::SPACE_TABLE_FRAMES
        + 1
        + TASK_USER_STACK_PAGES
        + parts.image.as_ref()?.page_count()
        + heap_frames
        + mmap_frames;
    if !mem_policy::task_charge_ok(0, ledger_frames, task_frame_limit()) {
        return None;
    }

    let Some(kernel_stack_start) = kernel_stack::alloc() else {
        return None;
    };
    // The child resumes through `silver_fork_bootstrap`, so its kernel stack
    // starts with a copy of the parent's SVC frame and a zero `x0`.
    let frame_address =
        phys_to_virt(kernel_stack_start + TASK_STACK_SIZE) - 272;
    unsafe {
        core::ptr::copy_nonoverlapping(
            frame.as_ptr() as *const u8,
            frame_address as *mut u8,
            264,
        );
        core::ptr::write_bytes((frame_address + 264) as *mut u8, 0, 8);
        (frame_address as *mut u64).write(0);
    }
    let mut user_stack_pointer: u64;
    unsafe {
        core::arch::asm!("mrs {}, sp_el0", out(reg) user_stack_pointer);
    }

    let (stack_start, stack_generation) = parts.stack.take()?;
    let (private_page, private_generation) = parts.private.take()?;
    let Some((ttbr0, token)) = mmu::create_task_space(private_page, stack_start, None, true) else {
        kernel_stack::free(kernel_stack_start); return None;
    };
    parts.space = Some((token, mmu::asid_of(ttbr0) as u32));
    // The space owns the private page now; the stack is data, not a page
    // table, so keep it in the guard for failure cleanup and take it again at
    // publication.
    parts.stack = Some((stack_start, stack_generation));

    if !mmu::install_linux_image(ttbr0, parts.image.as_ref()?) {
        kernel_stack::free(kernel_stack_start); return None;
    }
    let heap_physical: Vec<usize> = parts.heap.iter().map(|&(page, _)| page).collect();
    if !heap_physical.is_empty() && !mmu::map_shared_user_heap_pages(ttbr0, 0, &heap_physical) {
        kernel_stack::free(kernel_stack_start); return None;
    }
    for region in &parts.mmap {
        let mapped = if region.lazy {
            let mut ok = true;
            for (&offset, &(page, _generation)) in &region.pages {
                if !mmu::map_user_page_at(
                    ttbr0,
                    region.virtual_address + offset as usize * PAGE_SIZE,
                    page,
                    region.prot,
                ) {
                    ok = false;
                    break;
                }
            }
            ok
        } else if !region.frames.is_empty() {
            // A paged file-backed region shares the parent's frames; each
            // frame is mapped separately. Mapping `physical_start` (zero for
            // this variant) would point every page at physical 0, which is
            // exactly how a dynamically linked child lost libc's text after
            // fork. A writable mapping starts read-only so the first write
            // faults into the per-page COW handler instead of corrupting the
            // frame the parent still uses.
            let child_prot = if region.prot & mmu::PROT_WRITE != 0 {
                mmu::PROT_READ
            } else {
                region.prot
            };
            let mut ok = true;
            for (index, &(page, _generation)) in region.frames.iter().enumerate() {
                if !mmu::map_user_page_at(
                    ttbr0,
                    region.virtual_address + index * PAGE_SIZE,
                    page,
                    child_prot,
                ) {
                    ok = false;
                    break;
                }
            }
            ok
        } else if region.shared.is_some() {
            mmu::map_shared_writable_pages_at(
                ttbr0,
                region.virtual_address,
                region.physical_start,
                region.page_count,
                region.prot,
            )
        } else {
            mmu::map_shared_user_pages_at(
                ttbr0,
                region.virtual_address,
                region.physical_start,
                region.page_count,
                region.prot,
            )
        };
        if !mapped {
            kernel_stack::free(kernel_stack_start); return None;
        }
    }

    // The child now maps the same frames as the parent. Flip both address
    // spaces to read-only so the first write on either side faults into the
    // COW handler. Text/rodata stays as it was.
    {
        let parent = manager.tasks.get_mut(parent_index)?;
        let pages_ok = mmu::protect_user_pages_at(
            parent.ttbr0,
            mmu::USER_STACK_VA,
            TASK_USER_STACK_PAGES,
            mmu::PROT_READ,
        ) && mmu::protect_private_page(parent.ttbr0)
            && (parent_heap.is_empty()
                || mmu::protect_user_pages_at(
                    parent.ttbr0,
                    mmu::USER_HEAP_VA,
                    parent_heap.len(),
                    mmu::PROT_READ,
                ));
        if !pages_ok {
            kernel_stack::free(kernel_stack_start); return None;
        }
        for region in &parent_mmap {
            if region.shared.is_some() {
                // Shared windows stay writable on both sides.
                continue;
            }
            if region.lazy {
                // Nothing is committed on either side yet; faults allocate
                // private frames in whichever address space touches first.
                continue;
            }
            if !region.frames.is_empty() {
                // The child shares these frames; make the parent read-only
                // too so either side's first write copies one page.
                if region.prot & mmu::PROT_WRITE != 0
                    && !mmu::protect_user_pages_at(
                        parent.ttbr0,
                        region.virtual_address,
                        region.page_count,
                        mmu::PROT_READ,
                    )
                {
                    kernel_stack::free(kernel_stack_start);
                    return None;
                }
                continue;
            }
            if region.prot & mmu::PROT_WRITE != 0
                && !mmu::protect_user_pages_at(
                    parent.ttbr0,
                    region.virtual_address,
                    region.page_count,
                    mmu::PROT_READ,
                )
            {
                kernel_stack::free(kernel_stack_start);
                return None;
            }
        }
        if !mmu::protect_cow_image(parent.ttbr0, parts.image.as_ref()?) {
            kernel_stack::free(kernel_stack_start); return None;
        }
        parent.linux_image.as_mut()?.mark_data_cow();
    }

    let child_id = manager.next_id;
    // Duplicate descriptors last: no fallible step may follow this.
    if !vfs::dup_owner(parent_id, child_id) {
        kernel_stack::free(kernel_stack_start); return None;
    }
    crate::linux_fd::fork_table(parent_id, child_id);

    let mut context = TaskContext::zero();
    context.stack_pointer = frame_address as u64;
    context.user_stack_pointer = user_sp.unwrap_or(user_stack_pointer);
    context.elr = silver_fork_bootstrap as *const () as usize as u64;
    context.tls = parent_tls;

    let (space_token, space_asid) = parts.space.take().unwrap();
    let _ = space_asid;
    let (stack_start, stack_generation) = parts.stack.take()?;

    let task = Task {
        fs_write_prefix: parent_prefix,
        fs_quota: parent_quota,
        read_prefix: parent_read_prefix,
        owns_space: true,
        tgid: child_id,
        pid_one: false,
        is_thread: false,
        clear_child_tid: None,
        id: child_id,
        state: TaskState::Ready,
        resume_state: None,
        program: user_program::PROGRAM_DYNAMIC,
        argument: 0,
        personality: Personality::Linux,
        cap_table: capability::CapTable::new(),
        bound_irq: None,
        irq_pending: false,
        arguments: parent_arguments,
        ppid: Some(parent_id),
        exit_status: None,
        resources_released: false,
        argv: parent_argv,
        envp: parent_envp,
        cwd: parent_cwd,
        owns_console: false,
        priority: parent_priority,
        context,
        fp: parent_fp,
        stack: MemoryRegion::new(kernel_stack_start, kernel_stack_start + TASK_STACK_SIZE),
        user_stack: MemoryRegion::new(stack_start, stack_start + TASK_USER_STACK_SIZE),
        user_stack_generation: stack_generation,
        private_page,
        private_page_generation: private_generation,
        space_token,
        ttbr0,
        user_image: None,
        linux_image: parts.image.take(),
        heap_pages: core::mem::take(&mut parts.heap),
        dma_pages: Vec::new(),
        mmap_regions: core::mem::take(&mut parts.mmap),
        mmap_bytes: parent_mmap_bytes,
        signal_trampoline: parent_trampoline,
        mem_frames: ledger_frames,
        program_break: parent_break,
        waiting_on: None,
        owned_mutexes: Vec::new(),
        running_on: None,
        is_idle: false,
    };

    manager.tasks.push(task);
    manager.next_id += 1;
    let index = manager.tasks.len() - 1;
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(child_id)
}

/// Result of `wait4`.
#[cfg(feature = "linux-abi")]
pub enum WaitChild {
    Exited(u64, u64),
    StillRunning,
    NoChild,
}

/// Waits for one child of the calling task (`wait4`). `pid <= 0` matches any
/// child; a positive value matches exactly that pid.
#[cfg(feature = "linux-abi")]
pub fn wait_child(pid: i64, nohang: bool) -> WaitChild {
    // A thread waits on behalf of its whole process: the child of a fork
    // performed by any thread is parented to the group leader.
    let parent = current_tgid();
    // `wait4(-1)` must return any *terminated* child first. Blocking on the
    // first child in table order stalls the caller (an interactive shell
    // waiting on a foreground command) while an unrelated background child
    // keeps running, so an unqualified wait polls for a zombie instead of
    // parking inside one child's wait_inner forever.
    let matches = |task: &Task| {
        task.ppid == Some(parent) && (pid <= 0 || task.id == pid as u64)
    };
    loop {
        let Some(manager) = scheduler_manager() else {
            return WaitChild::NoChild;
        };
        let terminated = manager
            .tasks
            .iter()
            .find(|task| matches(task) && task.state == TaskState::Zombie)
            .map(|task| task.id);
        // A `Dead` child becomes `Zombie` in the reaper; until then it counts
        // as still running for wait purposes.
        let running = manager
            .tasks
            .iter()
            .any(|task| matches(task) && task.state != TaskState::Zombie);
        let Some(id) = terminated else {
            if !running {
                return WaitChild::NoChild;
            }
            if nohang {
                return WaitChild::StillRunning;
            }
            if pid > 0 {
                // A qualified wait blocks on that child specifically.
                return match wait_inner(pid as u64, false, true) {
                    Some(WaitEvent::Exited(status)) => WaitChild::Exited(pid as u64, status),
                    _ => WaitChild::NoChild,
                };
            }
            // The borrow ends before sleeping; no guard is held here.
            sleep(1);
            continue;
        };
        return match wait_inner(id, false, true) {
            Some(WaitEvent::Exited(status)) => WaitChild::Exited(id, status),
            _ => WaitChild::NoChild,
        };
    }
}

pub fn capability_task_id(cap_id: capability::CapId) -> Option<u64> {
    capability_task_id_with_rights(cap_id, 0)
}

pub fn capability_task_id_with_rights(cap_id: capability::CapId, rights: u32) -> Option<u64> {
    let cap = current_task_mut()?.cap_table.get(cap_id)?;
    if cap.rights & rights != rights {
        return None;
    }
    Some(cap.target_task)
}

/// Delegates `rights` from the caller's `cap_id` to `recipient_task_id`.
///
/// The request is all-or-nothing. The source capability must cover every
/// requested bit, and a request that mixes global bits (`CAP_MMIO`, `CAP_IRQ`,
/// `CAP_PCI`, `CAP_MOUNT`) with target-scoped bits is rejected entirely when
/// the caller does not own *all* of the requested global bits itself. Nothing
/// is partially applied: a spawn handle over a child can pass on `CAP_KILL`,
/// but a `CAP_KILL | CAP_MMIO` request through that handle grants neither.
///
/// Global rights become the recipient's own self-capability; target-scoped
/// rights stay bound to the original capability's target.
pub fn grant_capability(cap_id: capability::CapId, recipient_task_id: u64, rights: u32) -> bool {
    let known = capability::CAP_ALL | capability::CAP_MOUNT | capability::CAP_POWER;
    if rights == 0 || rights & !known != 0 {
        return false;
    }
    // DMA sharing is not delegated through a generic handle: the owner must
    // name the physical region with `SYS_DMA_GRANT_SELF`. A handle covers a
    // task, not a range, and an owner-wide grant was the F2 hole.
    if rights & capability::CAP_DMA_SHARE != 0 {
        return false;
    }
    let caller_id = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some((source_target, _, _)) =
        current_task_mut().and_then(|task| task.cap_table.grant(cap_id, rights))
    {
        let global = rights & capability::CAP_GLOBAL_BITS;
        let scoped = rights & capability::CAP_TARGET_BITS;
        // A process-wide right may only be delegated by a task that owns it
        // itself. A spawn handle over a child covers CAP_ALL bits but is not
        // an own privilege of the parent, so it cannot pass device/mount
        // rights on.
        let owns_global = global == 0
            || current_task_mut()
                .map(|task| task.cap_table.has_self_rights(caller_id, global))
                .unwrap_or(false);
        if owns_global {
            if let Some(manager) = scheduler_manager() {
                if let Some(recipient) = manager.tasks.iter_mut().find(|task| {
                    task.id == recipient_task_id
                        && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                }) {
                    // Authority may only be handed down: to yourself or to a
                    // task you spawned. Otherwise a holder of any handle could
                    // spread its rights to arbitrary pids.
                    if recipient_task_id != caller_id && recipient.ppid != Some(caller_id) {
                        interrupts::restore_irq(saved_daif);
                        return false;
                    }
                    // A repeated grant must not grow one task's table without
                    // limit; the whole request fails if there is no room.
                    let needed = usize::from(global != 0) + usize::from(scoped != 0);
                    if recipient.cap_table.has_capacity_for(needed) {
                        // Global rights become the recipient's own
                        // self-capability; target-scoped rights stay bound to
                        // the original target.
                        if global != 0 {
                            recipient.cap_table.insert(recipient_task_id, global);
                        }
                        if scoped != 0 {
                            recipient.cap_table.insert(source_target, scoped);
                        }
                        granted = true;
                    }
                }
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    granted
}

/// `SYS_DMA_GRANT_SELF(recipient, base, size)`: the owner of a DMA run hands
/// `recipient` the right to map exactly `[base, base + size)` of it. This is
/// the owner's consent to share its own memory, not delegation of authority
/// over another task, so the "self or own child" rule does not apply: a
/// server may hand a client a window it owns. The range is checked against
/// the caller's own `dma_pages`, so no task can grant access to someone
/// else's run.
pub fn dma_grant_self(recipient_task_id: u64, base: usize, size: usize) -> bool {
    if size == 0 {
        return false;
    }
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    let caller_id = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some(manager) = scheduler_manager() {
        let owns = manager
            .tasks
            .get(smp::current_index())
            .is_some_and(|task| {
                task.id == caller_id
                    && task.dma_pages.iter().any(|&(run_base, page_count, _)| {
                        run_base
                            .checked_add(page_count * PAGE_SIZE)
                            .is_some_and(|run_end| base >= run_base && end <= run_end)
                    })
            });
        if owns {
            if let Some(recipient) = manager.tasks.iter_mut().find(|task| {
                task.id == recipient_task_id
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            }) {
                if recipient.cap_table.has_capacity() {
                    recipient.cap_table.insert_region(
                        caller_id,
                        capability::CAP_DMA_SHARE,
                        Some((base as u64, size as u64)),
                    );
                    granted = true;
                }
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    granted
}

/// `SYS_MMIO_GRANT(recipient, base, size)`: delegate a sub-range of a device
/// mapping the caller already owns. Used by the trusted shell to hand the GIC
/// distributor range to the IRQ probe; a future DT broker uses the same rule
/// for driver children. Authority only flows down (self or own child).
pub fn mmio_grant(recipient_task_id: u64, base: usize, size: usize) -> bool {
    if size == 0 {
        return false;
    }
    let caller_id = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some(manager) = scheduler_manager() {
        let owns = manager
            .tasks
            .get(smp::current_index())
            .is_some_and(|task| {
                task.id == caller_id
                    && task
                        .cap_table
                        .has_mmio_range(caller_id, base as u64, size as u64)
            });
        if owns {
            if let Some(recipient) = manager.tasks.iter_mut().find(|task| {
                task.id == recipient_task_id
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            }) {
                if recipient_task_id != caller_id && recipient.ppid != Some(caller_id) {
                    interrupts::restore_irq(saved_daif);
                    return false;
                }
                if recipient.cap_table.has_capacity() {
                    recipient.cap_table.insert_region(
                        recipient_task_id,
                        capability::CAP_MMIO,
                        Some((base as u64, size as u64)),
                    );
                    granted = true;
                }
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    granted
}

/// `SYS_MMIO_CLAIM(base, size)`: a PCI-capable driver reserves a BAR range
/// within the platform window and receives a region-scoped `CAP_MMIO` for it.
/// Overlapping claims by another live task are refused; a dying task's claims
/// are released by the teardown helper.
pub fn mmio_claim(base: usize, size: usize) -> bool {
    let Some(task) = current_task_mut() else {
        return false;
    };
    let task_id = task.id;
    if !task
        .cap_table
        .has_self_rights(task_id, capability::CAP_PCI)
    {
        return false;
    }
    if !task.cap_table.has_capacity() {
        return false;
    }
    if !crate::driver_resources::claim_mmio(task_id, base as u64, size as u64) {
        return false;
    }
    task.cap_table
        .insert_region(task_id, capability::CAP_MMIO, Some((base as u64, size as u64)));
    true
}

/// `SYS_CAP_FIND(target, rights)`: id of the caller's first capability for
/// `target` covering `rights`, or 0. Lets a task delegate a scoped right it
/// received without having to remember the CapId.
pub fn cap_find_for_target(target: u64, rights: u32) -> u64 {
    current_task_mut()
        .and_then(|task| task.cap_table.find_for_target(target, rights))
        .unwrap_or(0)
}

/// Whether the current task may lend the console to a child it is spawning.
///
/// The console owner can (that is what owning it means), and so can a task that
/// holds `CAP_CONSOLE_GRANT` directly. `SYS_CONSOLE_SET` checks a capability
/// *handle* instead, which every spawner has for its own child; this check is
/// about the caller's own position, so a plain driver cannot grab the console
/// by spawning a helper.
pub fn current_task_can_lend_console() -> bool {
    current_task_mut().map_or(false, |task| {
        let id = task.id;
        task.owns_console || task.cap_table.has_self_rights(id, capability::CAP_CONSOLE_GRANT)
    })
}

pub fn current_task_has_rights(rights: u32) -> bool {
    current_task_mut()
        .map(|task| {
            let id = task.id;
            task.cap_table.has_self_rights(id, rights)
        })
        .unwrap_or(false)
}

pub fn current_resource_counts() -> Option<(usize, usize, usize, usize, usize)> {
    current_task_mut().map(|task| {
        let dma_pages = task.dma_pages.iter().map(|&(_, pages, _)| pages).sum();
        (task.mem_frames, task.heap_pages.len(), task.mmap_bytes, dma_pages, task.cap_table.len())
    })
}

/// True when the current task may write `path`: it either holds the global
/// `CAP_FS_WRITE`, or its bundle manifest granted a matching prefix.
pub fn fs_write_allowed(path: &str) -> bool {
    current_task_mut()
        .map(|task| {
            let id = task.id;
            if task.cap_table.has_self_rights(id, capability::CAP_FS_WRITE) {
                return true;
            }
            let Some(scope) = task.fs_write_prefix.as_deref() else {
                return false;
            };
            path == scope
                || (path.starts_with(scope) && path.as_bytes().get(scope.len()) == Some(&b'/'))
        })
        .unwrap_or(false)
}

/// Current task's per-bundle quota `(blocks, inodes)`, if its manifest set one.
pub fn fs_quota_limits() -> Option<(u32, u32)> {
    current_task_mut().and_then(|task| task.fs_quota)
}

/// Global task-table snapshot for health reporting: `(live, zombies)`.
pub fn task_counts() -> (usize, usize) {
    scheduler_manager().map_or((0, 0), |manager| {
        let mut live = 0;
        let mut zombies = 0;
        for task in &manager.tasks {
            match task.state {
                TaskState::Zombie => zombies += 1,
                TaskState::Dead => {}
                _ => live += 1,
            }
        }
        (live, zombies)
    })
}

/// DMA pages that could not be proven safe to free (leaked by policy).
pub fn dma_leaked_pages() -> usize {
    DMA_LEAKED_FRAMES.load(Ordering::Relaxed)
}

/// DMA pages waiting out the quarantine window.
pub fn dma_quarantined_pages() -> usize {
    DMA_QUARANTINED_FRAMES.load(Ordering::Relaxed)
}

/// Consults the current task's quota before a mutation that would grow its
/// write-prefix subtree. Trusted tasks with their own `CAP_FS_WRITE` are never
/// limited. A quota that cannot be accounted (`None` from the backend) fails
/// closed so an unsupported backend cannot silently bypass it.
pub fn fs_quota_allows(add_blocks: u32, add_inodes: u32) -> bool {
    let Some(task) = current_task_mut() else {
        return false;
    };
    let id = task.id;
    if task.cap_table.has_self_rights(id, capability::CAP_FS_WRITE) {
        return true;
    }
    let Some((max_blocks, max_inodes)) = task.fs_quota else {
        return true;
    };
    let Some(prefix) = task.fs_write_prefix.as_deref() else {
        return false;
    };
    let Some(usage) = vfs::usage("/", prefix) else {
        return false;
    };
    usage
        .blocks()
        .checked_add(add_blocks)
        .is_some_and(|total| total <= max_blocks)
        && usage
            .inodes
            .checked_add(add_inodes)
            .is_some_and(|total| total <= max_inodes)
}

/// Maps the host-shared ivshmem window into the current Linux task. The BAR
/// is programmed once by [`crate::ivshmem::ensure`]; each task gets its own
/// Non-Cacheable user mapping in the MMIO window.
#[cfg(feature = "linux-abi")]
pub fn map_ivshmem(length: usize) -> Option<usize> {
    if length == 0 || length > crate::ivshmem::BAR_SIZE {
        return None;
    }
    let base = crate::ivshmem::ensure()?;
    with_group_owner(|task| mmu::map_phys_nc_to_user(task, base, length))?
}

/// Maps a DRM dumb buffer into the calling process. The caller has already
/// checked that the physical range belongs to a live dumb buffer.
pub fn map_drm_dumb(phys_addr: usize, length: usize) -> Option<usize> {
    with_group_owner(|task| mmu::map_shared_normal_to_user(task, phys_addr, length))?
}

/// Maps the virtio-gpu scanout framebuffer into the calling process. The
/// compositor writes frames straight into it and asks the kernel to flush.
/// The mapping is Normal Non-Cacheable, the same protocol as every other DMA
/// surface, so the device sees each write without a cache clean.
pub fn map_gpufb(length: usize) -> Option<usize> {
    let (base, size) = crate::virtio_gpu::framebuffer_phys()?;
    if length == 0 || length > size {
        return None;
    }
    with_group_owner(|task| mmu::map_shared_normal_to_user(task, base, length))?
}

pub fn map_phys_to_current(phys_addr: usize, size: usize) -> Option<usize> {
    map_phys_to_current_mode(phys_addr, size, false)
}

/// Maps a device-window range as Normal Non-Cacheable. Used by Wolfser's
/// ivshmem surface, which is host RAM rather than a register bank.
pub fn map_phys_nc_to_current(phys_addr: usize, size: usize) -> Option<usize> {
    map_phys_to_current_mode(phys_addr, size, true)
}

fn map_phys_to_current_mode(phys_addr: usize, size: usize, non_cacheable: bool) -> Option<usize> {
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let task_id = manager.tasks.get(smp::current_index())?.id;
        // Which task owns the DMA window this range falls into, if any?
        let region_owner = manager.tasks.iter().find_map(|owner| {
            owner
                .dma_pages
                .iter()
                .any(|&(base, page_count, _)| {
                    base.checked_add(page_count * PAGE_SIZE)
                        .is_some_and(|dma_end| {
                            phys_addr >= base
                                && phys_addr
                                    .checked_add(size)
                                    .is_some_and(|requested_end| requested_end <= dma_end)
                        })
                })
                .then_some(owner.id)
        });
        let task = manager.tasks.get_mut(smp::current_index())?;
        // Own DMA keeps the shared-DMA mapping. A foreign window requires the
        // target-scoped CAP_DMA_SHARE for its owner: CAP_MMIO alone (held by
        // several device tasks) must not become a cross-task memory read.
        let shared_dma = match region_owner {
            Some(owner) if owner == task_id => true,
            Some(owner) => {
                if !task
                    .cap_table
                    .has_dma_share(owner, phys_addr as u64, size as u64)
                {
                    return None;
                }
                true
            }
            None => false,
        };
        if !task
            .cap_table
            .has_self_rights(task_id, capability::CAP_MMIO)
        {
            return None;
        }
        // DMA windows are governed by ownership/share above. Anything else is
        // device MMIO and needs a region-scoped self capability: a broad
        // CAP_MMIO only opens the DMA path.
        if !shared_dma
            && !task
                .cap_table
                .has_mmio_range(task_id, phys_addr as u64, size as u64)
        {
            return None;
        }
        if !task.cap_table.has_capacity() {
            return None;
        }
        let virtual_address = if shared_dma {
            mmu::map_shared_dma_to_user(task, phys_addr, size)?
        } else if non_cacheable {
            mmu::map_phys_nc_to_user(task, phys_addr, size)?
        } else {
            mmu::map_phys_to_user(task, phys_addr, size)?
        };
        task.cap_table
            .insert(virtual_address as u64, capability::CAP_MMIO);
        Some(virtual_address)
    });
    interrupts::restore_irq(saved_daif);
    result
}

pub fn alloc_dma_for_current(size: usize) -> Option<(usize, usize, u32)> {
    alloc_dma_run(size)
}

/// Shared implementation for explicit DMA allocation. The capability is
/// checked before any frame is charged, so a refused request has no side
/// effects.
fn alloc_dma_run(size: usize) -> Option<(usize, usize, u32)> {
    if size == 0 || size > mmu::USER_DMA_MAX_SIZE {
        return None;
    }
    let page_count = size.div_ceil(PAGE_SIZE);
    let mapped_size = page_count * PAGE_SIZE;
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let task = manager.tasks.get_mut(smp::current_index())?;
        let task_id = task.id;
        if !task
            .cap_table
            .has_self_rights(task_id, capability::CAP_MMIO)
        {
            console::write_str("DMA: FAIL - missing MMIO right\r\n");
            return None;
        }
        if !task.charge_frames(page_count) {
            console::write_str("DMA: FAIL - per-task memory ledger full\r\n");
            return None;
        }
        let (physical, generation) =
            match frame_allocator::alloc_user_contiguous_pages_tagged(page_count) {
                Some(pair) => pair,
                None => {
                    console::write_str("DMA: FAIL - alloc_contiguous_pages returned None\r\n");
                    task.release_frames(page_count);
                    return None;
                }
            };
        // Every frame in the newly allocated run is recycled allocator memory.
        // `map_dma_to_user` clears the whole range through the cacheable
        // kernel alias and pushes it to the point of coherency before it
        // publishes the mapping to EL0 or the device.
        let Some(virtual_address) = mmu::map_dma_to_user(task, physical, mapped_size) else {
            console::write_str("DMA: FAIL - mmu::map_dma_to_user returned None\r\n");
            frame_allocator::free_contiguous_pages_tagged(physical, page_count, generation);
            task.release_frames(page_count);
            return None;
        };
        task.dma_pages.push((physical, page_count, generation));
        Some((virtual_address, physical, generation))
    });
    interrupts::restore_irq(saved_daif);
    result
}

pub fn bind_irq(intid: u32) -> bool {
    // Only SPIs are handed to userspace. SGIs/PPIs are banked per PE and
    // include the kernel's reschedule SGI and timer PPI; the capability
    // check below is necessary but not sufficient on its own.
    if !crate::irq_policy::is_user_bindable(intid, crate::gic::timer_irq()) {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager()
        .map(|manager| {
            let current_index = smp::current_index();
            let Some(current) = manager.tasks.get(current_index) else {
                return false;
            };
            if !current.cap_table.has_irq(current.id, intid) {
                return false;
            }
            if manager.tasks.iter().enumerate().any(|(index, task)| {
                index != current_index
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq == Some(intid)
            }) {
                return false;
            }
            let Some(task) = manager.tasks.get_mut(current_index) else {
                return false;
            };
            task.bound_irq = Some(intid);
            task.irq_pending = false;
            crate::gic::enable_irq(intid);
            true
        })
        .unwrap_or(false);
    interrupts::restore_irq(saved_daif);
    result
}

/// PID of the caller for parent tracking. Boot-spawned tasks (no current
/// task yet, or the idle task) get `None` instead of a fake parent.
fn current_task_pid() -> Option<u64> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.id)
        .filter(|&id| id != IDLE_TASK_ID)
}

/// True when `index` names a task this scheduler pass may dispatch right now:
/// in range, not terminal, not owned by a CPU, and not an idle task (idle is
/// only ever chosen by the global fallback so real work always wins).
fn ready_for_queue(manager: &TaskManager, index: usize) -> bool {
    manager.tasks.get(index).is_some_and(|task| {
        !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none()
    })
}

/// Appends `index` to `cpu`'s ready queue if it is dispatchable and not
/// already present. Deduplicating keeps the queues from growing when a task is
/// woken repeatedly without being dispatched.
fn enqueue_ready(manager: &mut TaskManager, cpu: usize, index: usize) {
    if !ready_for_queue(manager, index) {
        return;
    }
    let task = &manager.tasks[index];
    let id = task.id;
    let priority = task.priority;
    let cpu = cpu.min(smp::MAX_CPUS - 1);
    if !manager.ready[cpu]
        .iter()
        .any(|entry| entry.index == index && entry.id == id)
    {
        // Back of the queue: newest last, so same-class work stays FIFO. The
        // age starts at zero; a freshly readied task has not waited yet.
        manager.ready[cpu].push_back(ReadyEntry {
            id,
            index,
            priority,
            age: 0,
        });
    }
}

/// Pops the next dispatchable task from `cpu`'s queue, discarding stale
/// entries: a task that was killed, blocked again, claimed by another CPU, or
/// whose index was shifted by a removal (the stored id no longer matches).
///
/// Each call is one aging round: every entry that stays queued gains one
/// pass, so a low-class task is promoted even while high-class arrivals keep
/// the queue non-empty.
fn dequeue_ready(manager: &mut TaskManager, cpu: usize) -> Option<usize> {
    let cpu = cpu.min(smp::MAX_CPUS - 1);
    for entry in manager.ready[cpu].iter_mut() {
        entry.age = entry.age.saturating_add(1);
    }
    loop {
        let entry = pop_best(&mut manager.ready[cpu])?;
        let same_task = manager
            .tasks
            .get(entry.index)
            .map(|task| task.id == entry.id)
            .unwrap_or(false);
        if same_task && ready_for_queue(manager, entry.index) {
            return Some(entry.index);
        }
    }
}

/// Removes and returns the entry the policy would dispatch: the FIFO-first
/// entry with the best (lowest) effective class.
fn pop_best(queue: &mut VecDeque<ReadyEntry>) -> Option<ReadyEntry> {
    let mut best = None;
    let mut best_class = u8::MAX;
    for (position, entry) in queue.iter().enumerate() {
        let class = sched_policy::effective(entry.priority, entry.age);
        if class < best_class {
            best = Some(position);
            best_class = class;
        }
    }
    queue.remove(best?)
}

/// Rebuilds every queue from the task table. Required after any operation that
/// shifts indices (`Vec::remove` / `retain`); also used to seed the queues
/// after a state sweep.
fn rebuild_ready_queues(manager: &mut TaskManager) {
    for queue in &mut manager.ready {
        queue.clear();
    }
    let cpus = smp::cpu_count().max(1);
    // Split borrows: `tasks` is read while `ready` is written.
    let tasks = &manager.tasks;
    let ready = &mut manager.ready;
    for index in 0..tasks.len() {
        let task = &tasks[index];
        if !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none() {
            let cpu = index % cpus;
            ready[cpu].push_back(ReadyEntry {
                id: task.id,
                index,
                priority: task.priority,
                age: 0,
            });
        }
    }
}

/// True when `cpu` is online and currently runs its idle task. Read under the
/// kernel lock, which serialises every `current_index` mutation.
#[cfg(feature = "smp")]
fn cpu_is_idle(manager: &TaskManager, cpu: usize) -> bool {
    if !smp::cpu_online(cpu) {
        return false;
    }
    let index = smp::current_index_of(cpu);
    manager
        .tasks
        .get(index)
        .is_some_and(|task| task.is_idle)
}

/// Moves one queued task from `from` to an idle peer, if the policy finds a
/// worthwhile target. Called after a successful local dispatch: a CPU that
/// still holds work shares it instead of letting an idle PE sleep through a
/// timer window. The entry keeps its age, so a migrated low-class task does
/// not restart its aging.
#[cfg(feature = "smp")]
fn push_migrate(manager: &mut TaskManager, from: usize) -> bool {
    let cpus = smp::cpu_count().clamp(1, smp::MAX_CPUS);
    let mut idle = [false; smp::MAX_CPUS];
    for cpu in 0..cpus {
        idle[cpu] = cpu_is_idle(manager, cpu);
    }
    let moved = push_migrate_with(manager, from, &idle[..cpus]);
    if let Some(target) = moved {
        // The target may be parked in WFI or spinning on the kernel lock;
        // without an IPI the migrated task waits for the next timer tick.
        if let Some(mpidr) = smp::mpidr_of(target) {
            crate::gic::send_sgi_to(mpidr, crate::gic::RESCHEDULE_IRQ);
        }
        #[cfg(all(feature = "smp", feature = "smp-selftest"))]
        READY_PUSH_PICKS[target].fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// [`push_migrate`] with an explicit idle view. Kept separate so the boot test
/// can exercise the queue move deterministically: a live AP is usually parked
/// on the kernel lock with no current task at that moment, so "really idle"
/// cannot be observed from the boot CPU.
#[cfg(feature = "smp")]
fn push_migrate_with(manager: &mut TaskManager, from: usize, idle: &[bool]) -> Option<usize> {
    let from = from.min(smp::MAX_CPUS - 1);
    let cpus = idle.len().clamp(1, smp::MAX_CPUS);
    let mut loads = [0usize; smp::MAX_CPUS];
    for (cpu, load) in loads.iter_mut().enumerate().take(cpus) {
        *load = manager.ready[cpu].len();
    }
    let target = sched_policy::push_target(&loads[..cpus], idle, from)?;
    let entry = manager.ready[from].pop_back()?;
    manager.ready[target].push_back(entry);
    Some(target)
}

fn find_next_task(manager: &mut TaskManager, cpu: usize, scan_base: usize) -> Option<usize> {
    // 1. This CPU's own queue — the common case, O(1).
    if let Some(index) = dequeue_ready(manager, cpu) {
        #[cfg(all(feature = "smp", feature = "smp-selftest"))]
        note_pick(&READY_LOCAL_PICKS, cpu);
        #[cfg(feature = "smp")]
        push_migrate(manager, cpu);
        return Some(index);
    }
    // 2. Work stealing: take one dispatchable task from another CPU's queue.
    //    This can only move a task that is not currently owned by a CPU, so it
    //    never preempts a running task.
    for other in 0..smp::cpu_count() {
        if other != cpu {
            if let Some(index) = dequeue_ready(manager, other) {
                #[cfg(all(feature = "smp", feature = "smp-selftest"))]
                note_pick(&READY_STEAL_PICKS, cpu);
                return Some(index);
            }
        }
    }
    // 3. Fallback full scan: covers idle tasks and any Ready task whose enqueue
    //    was missed. Same "non-idle first" policy as before the queues.
    let task_count = manager.tasks.len();

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none() {
            #[cfg(all(feature = "smp", feature = "smp-selftest"))]
            note_pick(&READY_SCAN_PICKS, cpu);
            return Some(index);
        }
    }

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if task.state == TaskState::Ready && task.running_on.is_none() {
            #[cfg(all(feature = "smp", feature = "smp-selftest"))]
            note_pick(&READY_SCAN_PICKS, cpu);
            return Some(index);
        }
    }

    None
}

/// Detaches a task from the wait queue recorded in `waiting_on` and clears
/// the link. The task must still be in a blocked state when this runs.
///
/// Lifecycle invariant: a Ready/Running task carries no `waiting_on` and sits
/// in no WaitQueue; a Dead task is unlinked by the reap backstop. This helper
/// is the single place that restores the invariant when a blocked task leaves
/// its queue without having been woken by a notify.
///
/// Safety of the unlink itself is structural, see [`WaitLink`]: `Immortal`
/// queues are never dropped, `Shared` queues are kept alive by the waiter's
/// own `Arc` reference. Neither variant can name freed memory. Callers run
/// with IRQs masked inside `schedule()` on the single CPU.
fn detach_from_wait_queue(task: &mut Task) {
    match task.waiting_on.take() {
        Some(WaitLink::Immortal(queue)) => unsafe { (*queue).remove(task.id) },
        Some(WaitLink::Shared(condvar)) => condvar.remove_waiter(task.id),
        None => {}
    }
}

fn wake_expired_tasks(manager: &mut TaskManager, now: u64) {
    let mut became_ready: Vec<usize> = Vec::new();
    for (index, task) in manager.tasks.iter_mut().enumerate() {
        if task.state == TaskState::Stopped {
            let expired = match task.resume_state {
                Some(TaskState::Sleeping(deadline) | TaskState::BlockedUntil(deadline)) => {
                    now >= deadline
                }
                _ => false,
            };
            if expired {
                // A stopped task whose blocked-underneath deadline expired
                // must leave its queue before the resume is recorded: it is
                // still killable while Stopped, and reap must not find it
                // linked to a queue that a concurrent mailbox teardown could
                // free.
                if matches!(task.resume_state, Some(TaskState::BlockedUntil(_))) {
                    detach_from_wait_queue(task);
                }
                task.resume_state = Some(TaskState::Ready);
            }
            continue;
        }
        let wake_at = match task.state {
            TaskState::Sleeping(deadline) | TaskState::BlockedUntil(deadline) => deadline,
            _ => continue,
        };
        if now >= wake_at {
            // Timeout path: detach from the wait queue before becoming Ready.
            // Without this the task stays registered while Ready: a kill in
            // this window frees its own mailbox queue and reap then
            // dereferences the dangling `waiting_on` (use-after-free), and a
            // notify in this window burns its pop on an already-Ready task
            // (lost wakeup).
            if matches!(task.state, TaskState::BlockedUntil(_)) {
                detach_from_wait_queue(task);
            }
            task.state = TaskState::Ready;
            became_ready.push(index);
        }
    }
    for index in became_ready {
        enqueue_ready(manager, smp::this_cpu_id(), index);
    }
}

fn reap_dead_tasks(manager: &mut TaskManager) {
    // Drain the DMA quarantine on every pass, including passes with no dead
    // task: the grace window is measured in these passes, and delaying the
    // reclaim until the next death would hold the frames indefinitely.
    reclaim_quarantined_dma();

    if !manager
        .tasks
        .iter()
        .any(|task| task.state == TaskState::Dead)
    {
        return;
    }

    // Snapshot every online CPU's current task: removals (zombie cap) shift
    // indices, and the local CPU must be remapped too.
    let cpu_current_ids = snapshot_current_ids(manager);
    let live_id = manager.tasks.get(smp::current_index()).map(|task| task.id);

    // A task killed while Running on another CPU stays executable until its
    // own CPU observes the death in `schedule` (live_is_dead) and switches
    // away; reaping it earlier would pull the address space out from under
    // the still-running context.
    //
    // TASK №5: revoke foreign shared-DMA mappings of every dying task BEFORE
    // any teardown. At this point every non-zombie address space in
    // `manager.tasks` still has intact page tables. Zombies keep no address
    // space (`ttbr0 == 0`) and are skipped rather than dereferenced.
    // Stop the other PEs only when a dead task actually owns DMA frames to
    // revoke; ordinary schedule passes must not pay for a world stop. If the
    // handshake times out, fall back to the conservative per-range gate and
    // leak rather than tearing a live space down.
    let needs_quiesce = manager.tasks.iter().any(|task| {
        task.state == TaskState::Dead && task.running_on.is_none() && !task.dma_pages.is_empty()
    });
    let quiesced = needs_quiesce && smp::quiesce_begin();
    let mut dma_revokes: Vec<(usize, usize, mmu::RevokeResult)> = Vec::new();
    for dead in manager
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Dead && task.running_on.is_none())
    {
        for &(physical, page_count, _) in &dead.dma_pages {
            let fail = mmu::RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
            let Some(span) = page_count.checked_mul(PAGE_SIZE) else {
                dma_revokes.push((physical, page_count, fail));
                continue;
            };
            let Some(phys_end) = physical.checked_add(span) else {
                dma_revokes.push((physical, page_count, fail));
                continue;
            };
            // Device contract: an armed lease means the driver has not yet
            // observed completion and stopped the queue, so the device may
            // still write here. Quarantine only bounds a *quiet* range; it
            // cannot stop a running device. Leak instead.
            if dma_range_in_flight(physical, span) {
                console::write_str("DMA revoke: range 0x");
                console::write_hex_u64(physical as u64);
                console::write_str("-0x");
                console::write_hex_u64(phys_end as u64);
                console::write_str(": device lease still armed - leaked\r\n");
                dma_revokes.push((physical, page_count, fail));
                continue;
            }
            // Quiescence: a range mapped by a task currently executing on
            // another CPU must not be revoked or freed underneath it. Abort
            // the whole range before the first PTE is cleared; the caller
            // leaks the frames. Non-running spaces are still safe to scan.
            let running_mapping = !quiesced
                && manager.tasks.iter().any(|task| {
                    task.running_on.is_some()
                        && task.ttbr0 != 0
                        && mmu::range_mapped_by(task.ttbr0, physical, phys_end)
                });
            if running_mapping {
                console::write_str("DMA revoke: range 0x");
                console::write_hex_u64(physical as u64);
                console::write_str("-0x");
                console::write_hex_u64(phys_end as u64);
                console::write_str(": deferred, live CPU mapping - leaked\r\n");
                dma_revokes.push((physical, page_count, fail));
                continue;
            }
            let mut aggregated = mmu::RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
            let mut affected_tasks = 0usize;
            for other in manager
                .tasks
                .iter()
                .filter(|task| !task.resources_released && task.ttbr0 != 0)
            {
                let result = mmu::revoke_phys_range(other.ttbr0, physical, phys_end);
                aggregated.unmapped += result.unmapped;
                aggregated.safe_to_free &= result.safe_to_free;
                if result.unmapped > 0 {
                    affected_tasks += 1;
                }
            }
            console::write_str("DMA revoke: range 0x");
            console::write_hex_u64(physical as u64);
            console::write_str("-0x");
            console::write_hex_u64(phys_end as u64);
            console::write_str(": unmapped ");
            console::write_dec_u64(aggregated.unmapped as u64);
            console::write_str(" from ");
            console::write_dec_u64(affected_tasks as u64);
            console::write_str(" task(s), safe_to_free=");
            console::write_str(if aggregated.safe_to_free {
                "true"
            } else {
                "false"
            });
            console::write_str("\r\n");
            dma_revokes.push((physical, page_count, aggregated));
        }
    }

    // Transition safely-dead tasks to Zombie, releasing executable resources
    // exactly once. `clear_resources` empties the heavy fields, so a zombie
    // carries only PID/PPID/exit status and small metadata until `wait`.
    let mut became_zombie: Vec<u64> = Vec::new();
    let mut lost_console: Vec<u64> = Vec::new();
    let mut drop_threads: Vec<u64> = Vec::new();
    let mut wake: Vec<u64> = Vec::new();
    for task in &mut manager.tasks {
        if task.state != TaskState::Dead || task.running_on.is_some() || task.resources_released {
            continue;
        }
        // Backstop unlink: ipc queues were already detached in `remove_task`
        // before their free, so a Dead task's pointer here is either None or
        // a `'static` queue (e.g. PROCESS_WAIT_QUEUE).
        detach_from_wait_queue(task);
        // `exit_group` and other cross-CPU kills mark a sibling Dead without
        // running its own exit path, so the per-task tables (mailboxes, Linux
        // descriptors, signal dispositions, futex waiters, VFS handles) would
        // stay allocated for the life of the machine. These helpers are
        // idempotent: a task that exited normally already cleared them here.
        #[cfg(feature = "linux-abi")]
        crate::linux_fd::close_task(task.id);
        #[cfg(feature = "linux-abi")]
        crate::linux_signal::close_task(task.id);
        #[cfg(feature = "linux-abi")]
        crate::linux_abi::futex_cleanup(task.id);
        crate::ipc::remove_task(task.id);
        vfs::close_task(task.id);
        wake.extend(task.release_mutexes());
        task.cap_table.remove_all();
        // A dead task's bound line stays enabled in the GIC until here. Mask
        // A dead task's line must be masked before the binding disappears:
        // otherwise the next interrupt arrives with no owner, takes the
        // unknown-INTID path and charges the unmaskable streak (R3). Every
        // path that clears `bound_irq` goes through the same helper.
        release_bound_irq(task);
        task.irq_pending = false;
        task.release_stack();
        task.release_space(&dma_revokes);
        task.clear_resources();
        task.state = TaskState::Zombie;
        if task.owns_console {
            lost_console.push(task.id);
        }
        became_zombie.push(task.id);
        if task.is_thread {
            drop_threads.push(task.id);
        }
    }
    if quiesced {
        smp::quiesce_end();
        // Parked CPUs resume in their SGI/BKL wait; kick them once so a task
        // made Ready during teardown is picked up even if its CPU returns to
        // user mode instead of scheduling immediately.
        smp::ipi_reschedule_others();
    }

    // A task killed from another CPU (exit_group, kill) never runs its own
    // exit path, so the reaper must return the console it held. Without this
    // the shell stayed alive but its prompt and output went to a dead owner.
    for id in lost_console {
        recover_console(manager, id);
    }

    // Orphans must point at a live reaper, never at a dead PID.
    for &dead_id in &became_zombie {
        reparent_children(manager, dead_id);
    }

    // Retire capability records targeting the new zombies, keeping CAP_WAIT so
    // a holder can still wait for them.
    for &dead_id in &became_zombie {
        for task in manager
            .tasks
            .iter_mut()
            .filter(|task| !matches!(task.state, TaskState::Dead | TaskState::Zombie))
        {
            task.cap_table.retire_for_target(dead_id);
        }
    }

    // Wake mutex waiters released by the teardown.
    for id in wake {
        let found = manager.tasks.iter().position(|task| {
            task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
        });
        if let Some(index) = found {
            wake_blocked_index(manager, index, smp::this_cpu_id());
        }
    }

    // Threads are not waitable. Their status is only used by the
    // CLONE_CHILD_CLEARTID futex, so drop the metadata record immediately
    // instead of letting it count against the zombie cap.
    for id in drop_threads {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            let _ = take_zombie(manager, index);
        }
    }

    // Bound unwaited zombies (metadata only). The oldest statuses are lost;
    // their executable resources are already gone.
    let zombie_count = manager
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Zombie)
        .count();
    if zombie_count > ZOMBIE_LIMIT {
        let mut zombie_ids: Vec<u64> = manager
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::Zombie)
            .map(|task| task.id)
            .collect();
        zombie_ids.sort_unstable();
        let drop_count = zombie_count - ZOMBIE_LIMIT;
        let drop_ids: Vec<u64> = zombie_ids.into_iter().take(drop_count).collect();
        manager
            .tasks
            .retain(|task| !(task.state == TaskState::Zombie && drop_ids.contains(&task.id)));
        // Indices shifted: rebuild the per-CPU queues from the new table.
        rebuild_ready_queues(manager);
    }

    remap_current_indices(manager, &cpu_current_ids, live_id);

    if !became_zombie.is_empty() {
        wake_process_waiters(manager);
    }
}

/// Fault forensics for a dying task: which mmap region covers (or nearly
/// covers) the faulting address, and the neighboring regions. Read-only, no
/// allocation, no locks — runs in exception context.
pub fn debug_fault_region(far: usize) {
    let Some(task) = current_task_mut() else {
        console::write_str("FAULTCTX no task\r\n");
        return;
    };
    console::write_str("FAULTCTX regions=");
    console::write_dec_u64(task.mmap_regions.len() as u64);
    console::write_str("\r\n");
    let mut near = 0;
    for region in &task.mmap_regions {
        let start = region.virtual_address;
        let end = start + region.page_count * PAGE_SIZE;
        if far + PAGE_SIZE * 16 < start || far >= end + PAGE_SIZE * 16 {
            continue;
        }
        near += 1;
        if near > 6 {
            break;
        }
        console::write_str("  region 0x");
        console::write_hex_u64(start as u64);
        console::write_str("-0x");
        console::write_hex_u64(end as u64);
        console::write_str(" lazy=");
        console::write_dec_u64(region.lazy as u64);
        console::write_str(" phys=0x");
        console::write_hex_u64(region.physical_start as u64);
        console::write_str(" pages=");
        console::write_dec_u64(region.page_count as u64);
        console::write_str(" frames=");
        console::write_dec_u64(region.frames.len() as u64);
        console::write_str(" faultpages=");
        console::write_dec_u64(region.pages.len() as u64);
        if let Some((path, offset)) = &region.file {
            console::write_str(" file=");
            console::write_str(path);
            console::write_str("+");
            console::write_dec_u64(*offset as u64);
        }
        console::write_str("\r\n");
    }
    if near == 0 {
        console::write_str("  (none within 16 pages)\r\n");
    }
}

/// Lazy-fill failure records for `task_id`, reverse order.
pub fn debug_lazy_fail(task_id: u64, limit: usize) {
    let index = LAZY_FAIL_INDEX.load(core::sync::atomic::Ordering::Relaxed);
    if index == 0 {
        return;
    }
    let ring = unsafe { core::ptr::addr_of!(LAZY_FAIL).as_ref().unwrap() };
    let mut shown = 0;
    let mut cursor = index;
    while shown < limit && cursor > 0 {
        cursor -= 1;
        let slot = ring[cursor % LAZY_FAIL_LEN];
        if slot[5] != task_id {
            continue;
        }
        let tag = slot[1].to_le_bytes();
        console::write_str("  lazyfail page=0x");
        console::write_hex_u64(slot[0]);
        console::write_str(" reason=");
        console::write_str(core::str::from_utf8(&tag).unwrap_or("?"));
        console::write_str(" frames=");
        console::write_dec_u64(slot[2]);
        console::write_str(" mmap_bytes=");
        console::write_dec_u64(slot[3]);
        console::write_str(" regions=");
        console::write_dec_u64(slot[4]);
        console::write_str("\r\n");
        shown += 1;
    }
}

/// Last syscalls of `task_id` from the trace ring, reverse order.
#[cfg(feature = "linux-abi")]
pub fn debug_task_trace(task_id: u64, limit: usize) {
    // The ring is written by other CPUs. Read it through a raw pointer: a
    // shared reference to a mutable static would be UB even for a diagnostic
    // dump, and a torn record is acceptable here.
    let ring = core::ptr::addr_of!(crate::linux_abi::TRACE_RING)
        .cast::<[u64; 5]>();
    let index = crate::linux_abi::TRACE_RING_INDEX
        .load(core::sync::atomic::Ordering::Relaxed);
    let mut shown = 0;
    let mut cursor = index;
    while shown < limit && cursor > 0 {
        cursor -= 1;
        let slot = unsafe {
            core::ptr::read_volatile(ring.add(cursor % crate::linux_abi::TRACE_RING_LEN))
        };
        if slot[4] == task_id {
            console::write_str("  nr=");
            console::write_dec_u64(slot[0]);
            console::write_str(" a0=0x");
            console::write_hex_u64(slot[1]);
            console::write_str(" a1=0x");
            console::write_hex_u64(slot[2]);
            console::write_str(" ret=0x");
            console::write_hex_u64(slot[3]);
            console::write_str("\r\n");
            shown += 1;
        }
    }
}

#[cfg(not(feature = "linux-abi"))]
pub fn debug_task_trace(_task_id: u64, _limit: usize) {}

#[cfg(not(feature = "linux-abi"))]
pub fn fault_coverage(_address: usize) -> (u64, u64) {
    (0, 0)
}

#[cfg(not(feature = "linux-abi"))]
pub fn lazy_fail_record(
    _page: usize,
    _reason: &[u8],
    _mem_frames: usize,
    _mmap_bytes: usize,
    _regions: usize,
) {
}

#[cfg(not(feature = "linux-abi"))]
pub fn cow_diagnostic(_address: usize, _buffer: &mut [u8]) -> usize {
    0
}

fn current_task_mut() -> Option<&'static mut Task> {
    let manager = scheduler_manager()?;
    // Bounds-checked borrow. The former `get_mut(index).map(|_| &mut
    // *tasks.add(index))` dance minted a second `&mut` alongside the one
    // `get_mut` had already produced — aliasing UB the optimizer is free to
    // exploit (observed as a self-loop in `receive_matching` once the
    // per-CPU index load changed the codegen).
    manager.tasks.get_mut(smp::current_index())
}

/// Linux `auxv` entries for a loaded image.
///
/// `AT_RANDOM` and the terminating `AT_NULL` are appended by
/// `build_linux_stack`, so they are not listed here. `AT_PHDR` is zero when
/// the program headers are not covered by a `PT_LOAD`; static binaries that do
/// not consult it (musl's `_start`) are unaffected.
#[cfg(feature = "linux-abi")]
fn linux_auxv(image: AuxvImage) -> Vec<(u64, u64)> {
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_BASE: u64 = 7;
    const AT_FLAGS: u64 = 8;
    const AT_ENTRY: u64 = 9;
    const AT_UID: u64 = 11;
    const AT_EUID: u64 = 12;
    const AT_GID: u64 = 13;
    const AT_EGID: u64 = 14;
    const AT_SECURE: u64 = 23;
    const AT_EXECFN: u64 = 31;
    let mut entries = Vec::new();
    entries.push((AT_PHDR, image.phdr_address as u64));
    entries.push((AT_PHENT, image.phentsize));
    entries.push((AT_PHNUM, image.phnum));
    entries.push((AT_PAGESZ, PAGE_SIZE as u64));
    entries.push((AT_BASE, image.interp_base as u64));
    entries.push((AT_FLAGS, 0));
    entries.push((AT_ENTRY, image.program_entry as u64));
    entries.push((AT_UID, 0));
    entries.push((AT_EUID, 0));
    entries.push((AT_GID, 0));
    entries.push((AT_EGID, 0));
    entries.push((AT_SECURE, 0));
    // The stack builder replaces the zero with a pointer to argv[0].
    entries.push((AT_EXECFN, 0));
    entries
}

/// The subset of a loaded image that `auxv` needs, so the native and Linux
/// loaders share one builder.
#[cfg(feature = "linux-abi")]
#[derive(Copy, Clone)]
struct AuxvImage {
    entry: usize,
    program_entry: usize,
    interp_base: usize,
    phdr_address: usize,
    phentsize: u64,
    phnum: u64,
}

#[cfg(feature = "linux-abi")]
impl AuxvImage {
    fn native(image: &LoadedImage) -> Self {
        Self {
            entry: image.entry,
            program_entry: image.entry,
            interp_base: 0,
            phdr_address: image.phdr_address,
            phentsize: image.phentsize,
            phnum: image.phnum,
        }
    }

    fn linux(image: &LinuxImage) -> Self {
        Self {
            entry: image.entry,
            program_entry: image.program_entry,
            interp_base: image.interp_base,
            phdr_address: image.phdr_address,
            phentsize: image.phentsize,
            phnum: image.phnum,
        }
    }
}


/// Owned executable resources for one teardown. Split out of `Task` so
/// `execve` can release the *old* address space after the task already points
/// at the new one (the space must no longer be current when its tables are
/// retired).
struct SpaceResources {
    ttbr0: usize,
    space_token: TableToken,
    private_page: usize,
    private_page_generation: u32,
    mmap_regions: Vec<MmapRegion>,
    heap_pages: Vec<(usize, u32)>,
    dma_pages: Vec<(usize, usize, u32)>,
    user_image: Option<LoadedImage>,
    #[cfg(feature = "linux-abi")]
    linux_image: Option<LinuxImage>,
}

/// Releases an address space and everything owned by it. The caller must have
/// stopped running on `resources.ttbr0` already.
fn release_space_resources(
    resources: &SpaceResources,
    dma_revokes: &[(usize, usize, mmu::RevokeResult)],
) {
    for region in &resources.mmap_regions {
        let unmapped = if region.lazy {
            mmu::unmap_user_pages_at_lenient(resources.ttbr0, region.virtual_address, region.page_count)
        } else {
            mmu::unmap_user_pages_at(resources.ttbr0, region.virtual_address, region.page_count)
        };
        if unmapped {
            if region.shared.is_some() {
                // The memfd owns these frames; dropping `region.shared` above
                // drops this mapping's reference to the file.
                continue;
            }
            if region.lazy {
                for &(page, generation) in region.pages.values() {
                    if !frame_allocator::free_page_tagged(page, generation) {
                        console::write_str("MMAP: release refused page 0x");
                        console::write_hex_u64(page as u64);
                        console::write_str(" - leaked\r\n");
                    }
                }
                continue;
            }
            if !region.frames.is_empty() {
                for &(page, generation) in &region.frames {
                    if !frame_allocator::free_page_tagged(page, generation) {
                        console::write_str("MMAP: release refused page 0x");
                        console::write_hex_u64(page as u64);
                        console::write_str(" - leaked\r\n");
                    }
                }
                continue;
            }
            frame_allocator::free_contiguous_pages_tagged(
                region.physical_start,
                region.page_count,
                region.generation,
            );
        } else {
            // A live PTE still references these frames; leaking is the only
            // safe outcome.
            MMAP_LEAKED_FRAMES.fetch_add(region.page_count, Ordering::Relaxed);
            console::write_str("MMAP LEAK: teardown could not unmap range 0x");
            console::write_hex_u64(if region.lazy {
                region.virtual_address as u64
            } else {
                region.physical_start as u64
            });
            console::write_str(" (");
            console::write_dec_u64(region.page_count as u64);
            console::write_str(" pages)\r\n");
        }
    }
    // Heap data pages carry their own tags; the address-space teardown frees
    // only the page tables and the private page.
    for &(page, generation) in &resources.heap_pages {
        if !frame_allocator::free_page_tagged(page, generation) {
            console::write_str("HEAP: release refused for page 0x");
            console::write_hex_u64(page as u64);
            console::write_str(" - leaked\r\n");
        }
    }
    if !mmu::destroy_task_space(
        resources.space_token,
        resources.private_page,
        resources.private_page_generation,
        mmu::asid_of(resources.ttbr0),
    ) {
        console::write_str("MMU: address space teardown was refused\r\n");
    }
    for &(physical, page_count, generation) in &resources.dma_pages {
        // Exact (physical, page_count) match: allocator ranges never overlap,
        // so a dead task's own range cannot be confused with another dead
        // task's range in the same reap pass.
        let record = dma_revokes
            .iter()
            .find(|(base, pages, _)| *base == physical && *pages == page_count);
        match record {
            Some((_, _, result)) if result.safe_to_free => {
                // Quarantine instead of freeing immediately: a device queue
                // may still be draining even though every CPU mapping is
                // gone. See `DMA_QUARANTINE_EPOCHS`.
                if !quarantine_dma_run(physical, page_count, generation) {
                    DMA_LEAKED_FRAMES.fetch_add(page_count, Ordering::Relaxed);
                    console::write_str("DMA LEAK: quarantine full, range 0x");
                    console::write_hex_u64(physical as u64);
                    console::write_str(" (");
                    console::write_dec_u64(page_count as u64);
                    console::write_str(" pages)\r\n");
                }
            }
            _ => {
                DMA_LEAKED_FRAMES.fetch_add(page_count, Ordering::Relaxed);
                console::write_str("DMA LEAK: range 0x");
                console::write_hex_u64(physical as u64);
                console::write_str(" orphaned (");
                console::write_dec_u64(page_count as u64);
                console::write_str(" pages, total leaked frames ");
                console::write_dec_u64(DMA_LEAKED_FRAMES.load(Ordering::Relaxed) as u64);
                console::write_str(")\r\n");
            }
        }
    }
    if let Some(image) = &resources.user_image {
        image.release();
    }
    #[cfg(feature = "linux-abi")]
    if let Some(image) = &resources.linux_image {
        image.release();
    }
}

fn scheduler_slot() -> &'static mut Option<TaskManager> {
    unsafe { &mut *SCHEDULER.0.get() }
}

fn scheduler_manager() -> Option<&'static mut TaskManager> {
    scheduler_slot().as_mut()
}

extern "C" fn idle_task() -> ! {
    // First dispatch on a CPU enters here with the kernel lock held by the
    // dispatching `schedule` (user-mode bootstrap drops it in asm; kernel
    // entries do it here). Idle never holds the lock again: it sleeps in
    // `wfi` so other CPUs can schedule while this CPU waits for an IRQ.
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    interrupts::enable_irq();
    loop {
        interrupts::wait_for_irq();
    }
}
