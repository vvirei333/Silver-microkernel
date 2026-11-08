use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use alloc::collections::VecDeque;
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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Ready,
    Running,
    Sleeping(u64),
    Blocked,
    BlockedUntil(u64),
    Stopped,
    Dead,
    /// Terminated, executable resources already released; the small record
    /// survives only until a permitted wait consumes its exit status.
    Zombie,
}

#[repr(C, align(16))]
pub struct TaskContext {
    registers: [u64; 12],
    stack_pointer: u64,
    user_stack_pointer: u64,
    elr: u64,
}

impl TaskContext {
    const fn zero() -> Self {
        Self {
            registers: [0; 12],
            stack_pointer: 0,
            user_stack_pointer: 0,
            elr: 0,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct TaskInfo {
    pub id: u64,
    pub state: u64,
    pub program: u64,
    pub argument: u64,
}

/// One anonymous mapping installed by `SYS_MMAP`. Kept sorted by
/// `virtual_address` so the first-fit allocator can scan in a single pass.
struct MmapRegion {
    virtual_address: usize,
    page_count: usize,
    physical_start: usize,
    generation: u32,
}

/// Syscall ABI a task is running on.
///
/// `Native` is the Silver ABI (`x8` = `SYS_*`). `Linux` is the AArch64 Linux
/// ABI: `x8` carries a Linux syscall number and the kernel translates it. The
/// personality is fixed when the task is spawned from its ELF image; it is not
/// switchable by the task itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Personality {
    Native,
    Linux,
}

pub struct Task {
    pub id: u64,
    pub state: TaskState,
    resume_state: Option<TaskState>,
    pub program: u64,
    pub argument: u64,
    /// Syscall ABI this task entered userspace on.
    personality: Personality,
    pub cap_table: capability::CapTable,
    /// Path prefix this task may write under, taken from the bundle manifest.
    /// Only meaningful when the task has no global `CAP_FS_WRITE`.
    fs_write_prefix: Option<String>,
    /// Optional `(block limit, inode limit)` for the write prefix. `None`
    /// means no per-bundle quota; trusted tasks are never limited.
    fs_quota: Option<(u32, u32)>,
    pub bound_irq: Option<u32>,
    irq_pending: bool,
    arguments: Vec<u8>,
    /// Parent PID recorded at spawn. Reparented to the live reaper when the
    /// parent dies, so an orphan is never left pointing at a dead PID.
    ppid: Option<u64>,
    /// Exit status written by exit/kill and consumed exactly once by wait.
    exit_status: Option<u64>,
    /// Set when the executable resources (stacks, image, address space, DMA,
    /// heap, cwd, argv/envp) have been released. A `Zombie` only keeps this
    /// metadata; `wait` removes the record.
    resources_released: bool,
    /// Versioned argv/envp delivered on the initial user stack.
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    cwd: String,
    owns_console: bool,
    /// True only for per-CPU idle tasks. Kept explicit because AP idle tasks
    /// are spawned after the boot tasks to preserve SCOUT_ID and the other
    /// spawn-order identities.
    is_idle: bool,
    /// Dispatch class (0 high, 1 normal, 2 low). Kernel-only: userspace tasks
    /// are always Normal, and aging lives in the ready queue, not here.
    priority: u8,
    pub context: TaskContext,
    pub stack: MemoryRegion,
    pub user_stack: MemoryRegion,
    pub ttbr0: usize,
    user_image: Option<LoadedImage>,
    /// A foreign (Linux ABI) image, mapped at its own virtual addresses.
    linux_image: Option<LinuxImage>,
    /// `(physical page, allocation generation)` for every `brk` page. The
    /// tag makes a stale teardown free fail instead of releasing a frame that
    /// was already recycled.
    heap_pages: Vec<(usize, u32)>,
    user_stack_generation: u32,
    private_page: usize,
    private_page_generation: u32,
    /// Root token for `ttbr0`: the only proof that teardown still owns this
    /// address space. A stale token is refused by the page-table registry.
    space_token: TableToken,
    dma_pages: Vec<(usize, usize, u32)>,
    mmap_regions: Vec<MmapRegion>,
    /// Sum of `mmap_regions` spans, bounded by [`MAX_MMAP_BYTES_PER_TASK`].
    /// Kept separately so the quota check is O(1), not a walk per request.
    mmap_bytes: usize,
    /// Frames this task has charged to its ledger: every frame it can pin,
    /// including the fixed address-space tables, stacks, image, heap, mmap and
    /// DMA. Bounded by `mem_policy::task_frame_limit`; the global user-frame
    /// reserve is the second, machine-wide bound.
    mem_frames: usize,
    program_break: usize,
    waiting_on: Option<WaitLink>,
    owned_mutexes: Vec<*mut mutex::MutexState>,
    /// CPU currently executing this task; `Some` exactly while the task is
    /// in `Running` state. Guarded by the kernel lock.
    running_on: Option<usize>,
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

/// Boot-time proof of the DMA quarantine window: a queued run stays allocated
/// for `DMA_QUARANTINE_EPOCHS` reclaim passes and is released on the next one.
/// The frame allocator is otherwise untouched by this test.
pub fn run_dma_quarantine_self_test() -> bool {
    const PAGES: usize = 2;
    let Some((physical, generation)) = frame_allocator::alloc_contiguous_pages_tagged(PAGES) else {
        return false;
    };
    if !quarantine_dma_run(physical, PAGES, generation) {
        frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
        return false;
    }
    let queued = frame_allocator::frame_in_use(physical);

    // One pass short of the window: the run must still be allocated.
    for _ in 0..(DMA_QUARANTINE_EPOCHS - 1) {
        reclaim_quarantined_dma();
    }
    let still_held = frame_allocator::frame_in_use(physical);
    reclaim_quarantined_dma();
    let released = !frame_allocator::frame_in_use(physical);

    queued && still_held && released
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

/// Boot-time proof of the lease gate: an armed range is reported busy at both
/// edges and becomes free once the handle drops.
pub fn run_dma_lease_self_test() -> bool {
    const PAGES: usize = 2;
    let Some((physical, generation)) = frame_allocator::alloc_contiguous_pages_tagged(PAGES) else {
        return false;
    };
    let span = PAGES * PAGE_SIZE;
    let Some(lease) = DmaLease::arm(physical, span) else {
        frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
        return false;
    };
    let busy = dma_range_in_flight(physical, span);
    let left_edge = dma_range_in_flight(physical - PAGE_SIZE, PAGES * PAGE_SIZE);
    let right_edge = dma_range_in_flight(physical + span - PAGE_SIZE, PAGE_SIZE);
    let disjoint = !dma_range_in_flight(physical + span, PAGE_SIZE);
    drop(lease);
    let released = !dma_range_in_flight(physical, span);
    let freed = frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
    busy && left_edge && right_edge && disjoint && released && freed
}

// ---------------------------------------------------------------------------
// SMP churn stress (selftest)
// ---------------------------------------------------------------------------

#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_ROUNDS: usize = 4;
#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_PER_ROUND: usize = 3;
/// Churn tasks get explicit ids above the user space (like AP idle tasks) and
/// the global `next_id` is restored around each spawn. User-space spawn logic
/// assumes task ids advance in lockstep with per-task cap-handle ids, so a
/// stress task must never shift the ids of a real child (SCOUT, block driver).
#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_ID_BASE: u64 = 1 << 41;
#[cfg(all(feature = "smp", feature = "smp-churn"))]
static CHURN_EXITED: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "smp", feature = "smp-churn"))]
static CHURN_ON_CPU: [AtomicUsize; smp::MAX_CPUS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
];

/// Spawns one churn task with an explicit high id, restoring `next_id` so a
/// real user spawn (SCOUT, block driver) keeps the id its cap handle expects.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
fn spawn_churn(explicit_id: u64) -> Option<u64> {
    let saved_next = {
        let manager = scheduler_manager()?;
        let saved = manager.next_id;
        manager.next_id = explicit_id;
        saved
    };
    let result = spawn_kernel(churn_task);
    if let Some(manager) = scheduler_manager() {
        manager.next_id = saved_next;
    }
    result
}

/// Kernel task entry used by the churn stress: exercise a yield (fast path)
/// or a timed block (timeout wake), count the CPU, then exit so another pass
/// reaps and tears the space down.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
extern "C" fn churn_task() -> ! {
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    // A first dispatch inherits the dispatcher's masked DAIF (the scheduling
    // pass keeps IRQs disabled across the context switch).
    interrupts::enable_irq();
    let cpu = smp::this_cpu_id();
    if cpu < smp::MAX_CPUS {
        CHURN_ON_CPU[cpu].fetch_add(1, Ordering::Relaxed);
    }
    if current_task_id() & 1 == 0 {
        yield_current();
    } else {
        sleep(1);
    }
    CHURN_EXITED.fetch_add(1, Ordering::Release);
    exit_current();
}

/// SMP churn: spawn a batch of short-lived kernel tasks from the boot CPU,
/// release the BKL so the APs actually run them, then reap and require every
/// one to have left `Running`. Each spawn builds a fresh address space (ASID
/// allocation + 9 table frames) and each exit tears it down, so a run exercises
/// ready-queue dispatch, work stealing, ASID recycling and index-shifting
/// reaping under real concurrency. The per-pass invariant audit fails the boot
/// if the scheduler state drifts.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn run_smp_churn_selftest() -> bool {
    if smp::cpu_count() <= 1 {
        return true;
    }
    let total = CHURN_ROUNDS * CHURN_PER_ROUND;
    CHURN_EXITED.store(0, Ordering::Relaxed);
    for counter in &CHURN_ON_CPU {
        counter.store(0, Ordering::Relaxed);
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let first_id = CHURN_ID_BASE;
    for offset in 0..total {
        let Some(id) = spawn_churn(first_id + offset as u64) else {
            console::write_str("SMP SELFTEST: churn FAIL (spawn)\r\n");
            return false;
        };
        if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
            // A kernel probe must never take the demo console.
            task.owns_console = false;
        }
    }
    let last_id = first_id + total as u64 - 1;

    // The boot CPU holds the BKL. Release it so the APs can run the tasks;
    // the boot task itself stays Running and cannot be claimed. Keep IRQs
    // enabled while waiting so this CPU can still acknowledge a peer's
    // stop-the-world SGI and take reschedule IPIs. A timer taken here
    // re-enters `schedule`, which refuses the nested pass and returns,
    // leaving this CPU's pass untouched; global time comes from the system
    // counter, so deadlines advance whether or not this CPU takes its PPI.
    smp::bkl_release();
    interrupts::enable_irq();
    let mut spins: u32 = 0;
    while CHURN_EXITED.load(Ordering::Acquire) < total {
        spins += 1;
        if spins >= 50_000_000 {
            break;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    interrupts::save_and_disable_irq();
    smp::bkl_acquire();

    let exited = CHURN_EXITED.load(Ordering::Acquire);

    // A Dead task still owned by a CPU is skipped by reap until that CPU
    // switches away, so drain in short windows with the lock released.
    let mut pending = total;
    for _ in 0..64 {
        reap_dead_tasks(manager);
        pending = manager
            .tasks
            .iter()
            .filter(|task| {
                task.id >= first_id && task.id <= last_id && task.state != TaskState::Zombie
            })
            .count();
        if pending == 0 {
            break;
        }
        smp::bkl_release();
        interrupts::enable_irq();
        for _ in 0..100_000 {
            unsafe {
                core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
            }
        }
        interrupts::save_and_disable_irq();
        smp::bkl_acquire();
    }

    let ran_cpus = (0..smp::MAX_CPUS)
        .filter(|&cpu| CHURN_ON_CPU[cpu].load(Ordering::Relaxed) > 0)
        .count();
    // With only one AP available the boot CPU cannot take part (it is the
    // waiter), so require one peer; with three APs require at least two. Use
    // the online count, not the possible-slot count: `-smp 2` must not be
    // judged against a four-slot policy.
    let min_cpus = if smp::online_cpu_count() >= 3 { 2 } else { 1 };
    let ok = exited == total && pending == 0 && ran_cpus >= min_cpus;
    if ok {
        console::write_str("SMP SELFTEST: churn PASS (");
        console::write_dec_u64(total as u64);
        console::write_str(" tasks, ");
        console::write_dec_u64(ran_cpus as u64);
        console::write_str(" CPU(s) ran)\r\n");
    } else {
        console::write_str("SMP SELFTEST: churn FAIL (exited=");
        console::write_dec_u64(exited as u64);
        console::write_str("/");
        console::write_dec_u64(total as u64);
        console::write_str(" pending=");
        console::write_dec_u64(pending as u64);
        console::write_str(" cpus=");
        console::write_dec_u64(ran_cpus as u64);
        console::write_str(")\r\n");
    }
    ok
}

/// Opt-in negative test for the stall watchdog: this task masks IRQs and
/// spins for longer than the watchdog grace window, exactly like a PE that
/// hangs in a non-scheduling loop. It then unmasks, yields and exits.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
extern "C" fn stall_probe_task() -> ! {
    // A first dispatch inherits the dispatcher's kernel-lock hold. Drop it:
    // the probe must simulate a CPU stuck with IRQs masked, not a CPU that
    // also owns the global lock (which would stall every other CPU and is a
    // different failure).
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    crate::interrupts::save_and_disable_irq();
    let start = crate::timer::ticks();
    // 120 ticks = 1.2 s at 100 Hz: three 250 ms check rounds plus margin.
    while crate::timer::ticks().saturating_sub(start) < 120 {
        core::hint::spin_loop();
    }
    crate::interrupts::enable_irq();
    yield_current();
    exit_current()
}

/// Proves the monitor end to end: spawn a kernel task that masks IRQs and
/// spins, let an AP pick it, and require another PE to report the stall. The
/// probe is retired afterwards and the boot continues.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn run_stall_selftest() -> bool {
    if smp::online_cpu_count() < 2 {
        return true;
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    const STALL_PROBE_ID: u64 = 1 << 44;
    let saved_next = manager.next_id;
    manager.next_id = STALL_PROBE_ID;
    let probe = spawn_kernel(stall_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
        // A diagnostic probe must never take the demo console.
        task.owns_console = false;
    }
    crate::watchdog::reset_for_test();

    // Release the BKL so an AP can dispatch the probe, and keep our own IRQs
    // enabled so this CPU runs its own check rounds.
    smp::bkl_release();
    interrupts::enable_irq();
    let detect_deadline = timer::ticks().saturating_add(200);
    while crate::watchdog::stall_count() == 0 && timer::ticks() < detect_deadline {
        core::hint::spin_loop();
    }
    let reported = crate::watchdog::stall_count() > 0;

    // The probe stops masking on its own; give it time to yield and exit.
    let cleanup_deadline = timer::ticks().saturating_add(100);
    while timer::ticks() < cleanup_deadline {
        core::hint::spin_loop();
    }
    interrupts::save_and_disable_irq();
    smp::bkl_acquire();

    let mut pending = true;
    for _ in 0..64 {
        reap_dead_tasks(manager);
        pending = manager
            .tasks
            .iter()
            .any(|task| task.id == id && task.state != TaskState::Zombie);
        if !pending {
            break;
        }
        smp::bkl_release();
        interrupts::enable_irq();
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        interrupts::save_and_disable_irq();
        smp::bkl_acquire();
    }
    if let Some(index) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, index);
    }
    let cleaned = manager.tasks.iter().all(|task| task.id != id);
    let ok = reported && cleaned && !pending;
    if ok {
        console::write_str("SMP SELFTEST: watchdog stall PASS (masked-IRQ probe detected)\r\n");
    } else {
        console::write_str("SMP SELFTEST: watchdog stall FAIL (reported=");
        console::write_dec_u64(reported as u64);
        console::write_str(" cleaned=");
        console::write_dec_u64(cleaned as u64);
        console::write_str(" pending=");
        console::write_dec_u64(pending as u64);
        console::write_str(")\r\n");
    }
    ok
}

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
    mov x0, x20
    msr elr_el1, x19
    mov x9, xzr
    msr spsr_el1, x9
    eret
    .size silver_user_bootstrap, . - silver_user_bootstrap
"#,
    bkl_release = sym crate::smp::bkl_release_asm
);

extern "C" {
    fn silver_context_switch(current: *mut TaskContext, next: *const TaskContext);
    fn silver_user_bootstrap();
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

/// Boot proof of partial `munmap`: map four pages, remove the middle two,
/// require the surviving ends to stay mapped and the hole to be gone, refill
/// the hole (first-fit), then release every remaining range and require the
/// ledger to return to its baseline.
pub fn run_mmap_split_selftest() -> bool {
    let Some(baseline) = current_task_mem_frames() else {
        return false;
    };
    let Some(ttbr0) = current_task_mut().map(|task| task.ttbr0) else {
        return false;
    };
    let page = PAGE_SIZE;
    let Some(base) = mmap_anonymous(4 * page, mmu::PROT_WRITE) else {
        return false;
    };
    let split = munmap(base + page, 2 * page);
    let split_regions = current_task_mmap_regions();
    let holes_gone = !mmu::user_page_is_mapped(ttbr0, base + page)
        && !mmu::user_page_is_mapped(ttbr0, base + 2 * page);
    let ends_kept = mmu::user_page_is_mapped(ttbr0, base)
        && mmu::user_page_is_mapped(ttbr0, base + 3 * page);
    // First-fit must put the next single page back into the hole.
    let refilled = mmap_anonymous(page, mmu::PROT_WRITE) == Some(base + page);
    let mut freed = refilled;
    if refilled {
        freed &= munmap(base + page, page);
        freed &= munmap(base, page);
        freed &= munmap(base + 3 * page, page);
    }
    let empty = current_task_mem_frames() == Some(baseline);
    let ok = split && holes_gone && ends_kept && refilled && freed && empty;
    #[cfg(not(feature = "tablet"))]
    {
        crate::console::write_str(if ok {
            "MMAP SPLIT PASS (regions="
        } else {
            "MMAP SPLIT FAIL (regions="
        });
        crate::console::write_dec_u64(split_regions as u64);
        crate::console::write_str(", ledger=");
        crate::console::write_dec_u64(current_task_mem_frames().unwrap_or(0) as u64);
        crate::console::write_str(")\r\n");
    }
    ok
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

/// Boot proof that the per-task ledger is wired: a two-frame charge fits a
/// forced limit exactly, one more frame is refused without changing the
/// counter, releasing restores the original value, and clearing the override
/// reopens the gate.
pub fn run_task_ledger_selftest() -> bool {
    let Some(before) = current_task_mem_frames() else {
        return false;
    };
    let limit = task_frame_limit();
    TASK_LIMIT_OVERRIDE.store(before.saturating_add(2), Ordering::Relaxed);
    let charged = charge_current_frames(2);
    let refused = !charge_current_frames(1);
    release_current_frames(2);
    let restored = current_task_mem_frames() == Some(before);
    TASK_LIMIT_OVERRIDE.store(usize::MAX, Ordering::Relaxed);
    let reopened = charge_current_frames(1);
    if reopened {
        release_current_frames(1);
    }
    let ok = charged && refused && restored && reopened && current_task_mem_frames() == Some(before);
    #[cfg(not(feature = "tablet"))]
    {
        crate::console::write_str(if ok {
            "TASK LEDGER PASS (limit="
        } else {
            "TASK LEDGER FAIL (limit="
        });
        crate::console::write_dec_u64(limit as u64);
        crate::console::write_str(" frames, before=");
        crate::console::write_dec_u64(before as u64);
        crate::console::write_str(")\r\n");
    }
    ok
}

/// Asserts the SYS_STOP_CONSOLE policy: the syscall requires
/// `CAP_CONSOLE_GRANT` on the caller. The console owner (HOWLER) receives it
/// at spawn, while a plain driver does not, so a driver is denied and the
/// owner is allowed. This guards the grant map; the syscall branch itself is
/// `current_task_has_rights(CAP_CONSOLE_GRANT)`.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_console_cap_selftest() -> bool {
    let allowed = initial_self_rights(user_program::HOWLER)
        .is_some_and(|rights| rights & capability::CAP_CONSOLE_GRANT != 0);
    let denied = initial_self_rights(user_program::NET_DRIVER)
        .is_some_and(|rights| rights & capability::CAP_CONSOLE_GRANT == 0);
    allowed && denied
}

/// Proves the ready queue is FIFO, not a stack. Three probes are spawned in
/// order and every queue is drained: the probes must appear in spawn order.
/// The old `Vec` + `pop()` pair dispatched the newest entry first, so with
/// three or more runnable tasks the oldest one was never dispatched at all.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn ready_queue_probe_task() -> ! {
    exit_current()
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_ready_queue_selftest() -> bool {
    const PROBE_BASE: u64 = 1 << 43;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let saved_next = manager.next_id;
    let mut ids = [0u64; 3];
    let mut spawned = 0usize;
    for (offset, slot) in ids.iter_mut().enumerate() {
        // Explicit ids keep `next_id` untouched: user-space spawn logic
        // assumes task ids and per-task cap-handle ids advance in lockstep.
        manager.next_id = PROBE_BASE + offset as u64;
        let Some(id) = spawn_kernel(ready_queue_probe_task) else {
            break;
        };
        *slot = id;
        spawned += 1;
    }
    manager.next_id = saved_next;

    // Pop every queue through the real `dequeue_ready` path (not `drain`,
    // which always walks front to back and would hide a stack-shaped queue),
    // record the dispatch order, then restore the snapshot.
    let mut seen = [usize::MAX; 3];
    let mut duplicates = false;
    let mut position = 0usize;
    for cpu in 0..smp::MAX_CPUS {
        let snapshot: Vec<ReadyEntry> = manager.ready[cpu].iter().copied().collect();
        while let Some(index) = dequeue_ready(manager, cpu) {
            let id = manager.tasks[index].id;
            if let Some(slot) = ids[..spawned].iter().position(|probe| *probe == id) {
                if seen[slot] != usize::MAX {
                    duplicates = true;
                }
                seen[slot] = position;
            }
            position += 1;
        }
        manager.ready[cpu].clear();
        manager.ready[cpu].extend(snapshot);
    }

    let ordered = spawned == 3
        && !duplicates
        && seen.iter().all(|&slot| slot != usize::MAX)
        && seen.windows(2).all(|pair| pair[0] < pair[1]);

    // The probes never ran; retire them exactly like a task that died before
    // its first dispatch (reap, then remove the zombie record).
    for &id in &ids[..spawned] {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            manager.tasks[index].state = TaskState::Dead;
            manager.tasks[index].running_on = None;
        }
    }
    reap_dead_tasks(manager);
    for &id in &ids[..spawned] {
        if let Some(index) = manager
            .tasks
            .iter()
            .position(|task| task.id == id && task.state == TaskState::Zombie)
        {
            take_zombie(manager, index);
        }
    }

    if ordered {
        console::write_str("SMP SELFTEST: ready fifo PASS (3 probes in spawn order)\r\n");
    } else {
        console::write_str("SMP SELFTEST: ready fifo FAIL (spawned=");
        console::write_dec_u64(spawned as u64);
        console::write_str(" positions=");
        console::write_dec_u64(seen[0] as u64);
        console::write_str(",");
        console::write_dec_u64(seen[1] as u64);
        console::write_str(",");
        console::write_dec_u64(seen[2] as u64);
        console::write_str(")\r\n");
    }
    ordered
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

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn priority_probe_task() -> ! {
    exit_current()
}

/// Proves the priority/aging contract on the real ready queue:
/// * a high-class probe enqueued last is dispatched before lower classes;
/// * two normal probes keep FIFO order;
/// * a low-class probe sitting behind a stream of fresh high-class arrivals
///   is promoted after two aging windows, so it cannot starve.
///
/// The queue is treated as scratch space: it is snapshotted before the probes
/// are spawned and restored afterwards, then the probes are retired.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_priority_selftest() -> bool {
    const PROBE_BASE: u64 = 1 << 45;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let cpu = smp::this_cpu_id().min(smp::MAX_CPUS - 1);
    let snapshot: Vec<ReadyEntry> = manager.ready[cpu].iter().copied().collect();

    let saved_next = manager.next_id;
    let mut ids = [0u64; 4];
    let mut spawned = 0usize;
    for (offset, slot) in ids.iter_mut().enumerate() {
        // Explicit ids keep `next_id` (and the user-space task-id/cap-handle
        // lockstep) untouched.
        manager.next_id = PROBE_BASE + offset as u64;
        let Some(id) = spawn_kernel(priority_probe_task) else {
            break;
        };
        *slot = id;
        spawned += 1;
    }
    manager.next_id = saved_next;

    let mut ok = spawned == 4;
    // Classes: low, normal, normal, high.
    if ok {
        ok = set_task_priority(ids[0], sched_policy::PRIORITY_LOW)
            && set_task_priority(ids[1], sched_policy::PRIORITY_NORMAL)
            && set_task_priority(ids[2], sched_policy::PRIORITY_NORMAL)
            && set_task_priority(ids[3], sched_policy::PRIORITY_HIGH);
    }

    // 1. Priority beats arrival order; same-class entries stay FIFO.
    let mut order = [0u64; 4];
    if ok {
        manager.ready[cpu].clear();
        for (slot, &id) in ids.iter().enumerate() {
            let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
                ok = false;
                break;
            };
            let priority = match slot {
                0 => sched_policy::PRIORITY_LOW,
                3 => sched_policy::PRIORITY_HIGH,
                _ => sched_policy::PRIORITY_NORMAL,
            };
            manager.ready[cpu].push_back(ReadyEntry {
                id,
                index,
                priority,
                age: 0,
            });
        }
        for slot in order.iter_mut() {
            let Some(index) = dequeue_ready(manager, cpu) else {
                ok = false;
                break;
            };
            *slot = manager.tasks[index].id;
        }
        ok &= order == [ids[3], ids[1], ids[2], ids[0]];
    }

    // 2. Bounded waiting: the low probe must win after two aging windows
    //    while a fresh high probe keeps being re-enqueued behind it.
    let mut aged_round = 0u32;
    if ok {
        let Some(low_index) = manager.tasks.iter().position(|task| task.id == ids[0]) else {
            return false;
        };
        let Some(high_index) = manager.tasks.iter().position(|task| task.id == ids[3]) else {
            return false;
        };
        manager.ready[cpu].clear();
        manager.ready[cpu].push_back(ReadyEntry {
            id: ids[0],
            index: low_index,
            priority: sched_policy::PRIORITY_LOW,
            age: 0,
        });
        manager.ready[cpu].push_back(ReadyEntry {
            id: ids[3],
            index: high_index,
            priority: sched_policy::PRIORITY_HIGH,
            age: 0,
        });
        for round in 1..=32u32 {
            let Some(index) = dequeue_ready(manager, cpu) else {
                ok = false;
                break;
            };
            if manager.tasks[index].id == ids[0] {
                aged_round = round;
                break;
            }
            // The high probe ran and yielded: put it back at the tail as a
            // fresh arrival, so pressure on the low probe stays constant.
            manager.ready[cpu].push_back(ReadyEntry {
                id: ids[3],
                index: high_index,
                priority: sched_policy::PRIORITY_HIGH,
                age: 0,
            });
        }
        ok &= aged_round > 0
            && aged_round <= (sched_policy::AGING_PASSES as u32 * 2 + 1);
    }

    // Restore the real queue and retire the probes.
    manager.ready[cpu].clear();
    manager.ready[cpu].extend(snapshot);
    for &id in &ids[..spawned] {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            manager.tasks[index].state = TaskState::Dead;
            manager.tasks[index].running_on = None;
        }
    }
    reap_dead_tasks(manager);
    for &id in &ids[..spawned] {
        if let Some(index) = manager
            .tasks
            .iter()
            .position(|task| task.id == id && task.state == TaskState::Zombie)
        {
            take_zombie(manager, index);
        }
    }

    if ok {
        console::write_str("SMP SELFTEST: priority PASS (high first, FIFO in class, low aged in ");
        console::write_dec_u64(aged_round as u64);
        console::write_str(" round(s))\r\n");
    } else {
        console::write_str("SMP SELFTEST: priority FAIL (spawned=");
        console::write_dec_u64(spawned as u64);
        console::write_str(" order=");
        console::write_dec_u64(order[0]);
        console::write_str(",");
        console::write_dec_u64(order[1]);
        console::write_str(",");
        console::write_dec_u64(order[2]);
        console::write_str(",");
        console::write_dec_u64(order[3]);
        console::write_str(" aged_round=");
        console::write_dec_u64(aged_round as u64);
        console::write_str(")\r\n");
    }
    ok
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn push_probe_task() -> ! {
    exit_current()
}

/// Proves push migration on the real queue: with one probe queued locally and
/// an idle, empty peer, `push_migrate` must move the probe to that peer. Skips
/// (returns true) when no peer is idle, which is legitimate for a profile that
/// keeps every CPU busy.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_push_selftest() -> bool {
    const PUSH_PROBE_ID: u64 = 1 << 46;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let me = smp::this_cpu_id();
    // Pick an online peer and mark it idle for the test. A live AP is usually
    // parked on the kernel lock with no current task here, so observing real
    // idleness would make this test timing-dependent; the real `push_migrate`
    // path uses `cpu_is_idle` and is visible in the `push=` dispatch counter.
    let Some(target) = (0..smp::MAX_CPUS)
        .find(|&cpu| cpu != me && cpu < smp::cpu_count() && smp::cpu_online(cpu))
    else {
        console::write_str("SMP SELFTEST: push SKIP (no online peer)\r\n");
        return true;
    };
    // Set the peer's queue aside (we hold the kernel lock, so it cannot drain
    // it) and restore it after the probe.
    let saved_target_queue: Vec<ReadyEntry> = manager.ready[target].drain(..).collect();

    let saved_next = manager.next_id;
    manager.next_id = PUSH_PROBE_ID;
    let probe = spawn_kernel(push_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    // A pass on another CPU cannot run while this CPU holds the kernel lock,
    // so the queue is stable: drop any copy spawn made and install exactly
    // one entry at the back of the local queue.
    for cpu in 0..smp::MAX_CPUS {
        manager.ready[cpu].retain(|entry| entry.id != id);
    }
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        return false;
    };
    manager.ready[me].push_back(ReadyEntry {
        id,
        index,
        priority: manager.tasks[index].priority,
        age: 0,
    });

    let cpus = smp::cpu_count().clamp(1, smp::MAX_CPUS);
    let mut idle = [false; smp::MAX_CPUS];
    idle[target] = true;
    let moved = push_migrate_with(manager, me, &idle[..cpus]).is_some();
    let on_target = manager.ready[target].iter().any(|entry| entry.id == id);
    let off_source = !manager.ready[me].iter().any(|entry| entry.id == id);

    // Undo the test's view of the target queue: drop the probe (it is retired
    // below) and put the peer's own entries back in their original order.
    manager.ready[target].retain(|entry| entry.id != id);
    manager.ready[target].extend(saved_target_queue);

    // Retire the probe before it can run.
    if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
        manager.tasks[index].state = TaskState::Dead;
        manager.tasks[index].running_on = None;
    }
    reap_dead_tasks(manager);
    if let Some(index) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, index);
    }

    let ok = moved && on_target && off_source;
    if ok {
        console::write_str("SMP SELFTEST: push PASS (probe moved to CPU");
        console::write_dec_u64(target as u64);
        console::write_str(")\r\n");
    } else {
        console::write_str("SMP SELFTEST: push FAIL (target=");
        console::write_dec_u64(target as u64);
        console::write_str(" moved=");
        console::write_dec_u64(moved as u64);
        console::write_str(" on_target=");
        console::write_dec_u64(on_target as u64);
        console::write_str(" off_source=");
        console::write_dec_u64(off_source as u64);
        console::write_str(")\r\n");
    }
    ok
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

pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        yield_current();
        return;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let wake_at = timer::ticks().saturating_add(timer::milliseconds_to_ticks(milliseconds));
    if let Some(task) = current_task_mut() {
        task.state = TaskState::Sleeping(wake_at);
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
        task.state = TaskState::Blocked;
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
        task.state = TaskState::Blocked;
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
        task.state = TaskState::BlockedUntil(deadline);
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

pub fn register_mutex(state: *mut mutex::MutexState) {
    if let Some(task) = current_task_mut() {
        if !task.owned_mutexes.contains(&state) {
            task.owned_mutexes.push(state);
        }
    }
}

pub fn unregister_mutex(state: *mut mutex::MutexState) {
    let Some(task) = current_task_mut() else {
        crate::console::write_str("MUTEX: unregister with no current task\r\n");
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

/// True when the scheduler may add another task record: an active slot is
/// free and the zombie/record metadata budget is not exhausted.
fn has_task_capacity(manager: &TaskManager) -> bool {
    if manager.tasks.len() >= MAX_TASK_RECORDS {
        return false;
    }
    manager
        .tasks
        .iter()
        .filter(|task| task.state != TaskState::Zombie)
        .count()
        < MAX_TASKS
}

/// Upper bound on a task's anonymous mappings. The virtual window allows far
/// more entries than the 128 MiB DRAM can back; this cap bounds the metadata
/// Vec itself, not just the frames.
const MAX_MMAP_REGIONS: usize = 128;

/// Per-task cap on anonymous mapping bytes. The mmap window is ~1 GiB but DRAM
/// is shared by every task, so one EL0 task must not be able to claim the whole
/// machine and starve the scheduler, IPC and the kernel heap. 16 MiB covers the
/// demo/`memtest` workloads with room to spare while keeping the global frame
/// allocator as the last line of defence rather than the first.
const MAX_MMAP_BYTES_PER_TASK: usize = 24 * 1024 * 1024;

pub fn exit_current() -> ! {
    exit_current_with_status(0)
}

pub fn exit_current_with_status(raw_status: u64) -> ! {
    // Reserve the top three u64 values for SYS_WAIT_EVENT sentinels
    // (ERROR/STOPPED/ECHILD). A user exit status must never alias them.
    let status = raw_status.min(u64::MAX - 4);
    // The syscall path already holds the kernel lock; the EL0 fault vector
    // enters without it. Acquire only when missing so the teardown below
    // (`ipc::remove_task` takes the IPC mutex, which asserts the BKL) is valid
    // in both paths. `schedule_released` balances the extra hold.
    if smp::bkl_depth() == 0 {
        smp::bkl_acquire();
    }
    let id = current_task_id();
    // Kernel-side trace: syscall::print is discarded for tasks that do not own
    // the console, so a task dying silently leaves no user-visible evidence.
    // console writes here are raw MMIO pokes and stay visible for every task.
    let program = current_task_mut().map_or(u64::MAX, |task| task.program);
    // Kernel tasks (program == u64::MAX) are internal probes; their exit is
    // not user-visible evidence and would only flood the console.
    if program != u64::MAX {
        console::write_str("EXIT: task=");
        console::write_dec_u64(id);
        console::write_str(" program=");
        console::write_dec_u64(program);
        console::write_str(" status=");
        console::write_dec_u64(status);
        // Packed reports (see the filesystem server's mount failure) are
        // unreadable in decimal, so the same value is repeated in hex.
        console::write_str(" (0x");
        console::write_hex_u64(status);
        console::write_str(")\r\n");
    }
    crate::ipc::remove_task(id);
    vfs::close_task(id);
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        task.state = TaskState::Dead;
        task.bound_irq = None;
        // The status survives the Dead -> Zombie transition in `reap`; the
        // executable resources are released there, after this CPU has left
        // the context (see `reap_dead_tasks`).
        task.exit_status = Some(status);
    }
    if let Some(manager) = scheduler_manager() {
        recover_console(manager, id);
        // A dying parent must not leave its children pointing at a dead PID.
        reparent_children(manager, id);
        wake_process_waiters(manager);
    }
    schedule_released();
    // A dead task must not park while holding the kernel lock. `schedule`
    // normally switches away here; when every other task is already running
    // (for example all idle tasks are busy) the pass returns without a switch,
    // and `schedule_released` has just re-acquired the hold. Drop whatever
    // remains before waiting for an interrupt, otherwise this CPU pins the
    // lock while it is parked.
    while smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

pub fn kill_task(id: u64) -> bool {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let Some(manager) = scheduler_manager() else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    if matches!(
        manager.tasks[index].state,
        TaskState::Dead | TaskState::Zombie
    ) {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    // Side effects only after the target is proven live: a bogus CAP_KILL
    // target must not broadcast mailbox/console wakeups across the system.
    crate::ipc::remove_task(id);
    vfs::close_task(id);
    manager.tasks[index].state = TaskState::Dead;
    manager.tasks[index].bound_irq = None;
    manager.tasks[index].exit_status = Some(1);
    recover_console(manager, id);
    reparent_children(manager, id);
    wake_process_waiters(manager);
    schedule_released();
    interrupts::restore_irq(saved_daif);
    true
}

fn recover_console(manager: &mut TaskManager, exited: u64) {
    let Some(exited_task) = manager.tasks.iter_mut().find(|task| task.id == exited) else {
        return;
    };
    if !exited_task.owns_console {
        return;
    }
    exited_task.owns_console = false;
    manager.console_parent = None;
    if cfg!(feature = "demo-tasks") {
        return;
    }
    let fallback = manager
        .tasks
        .iter()
        .find(|task| {
            !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program == user_program::HOWL
        })
        .map(|task| task.id);
    for task in &mut manager.tasks {
        task.owns_console = Some(task.id) == fallback;
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitEvent {
    Exited(u64),
    Stopped,
}

pub fn wait_for_task(id: u64) -> Option<u64> {
    match wait_inner(id, false, true) {
        Some(WaitEvent::Exited(status)) => Some(status),
        _ => None,
    }
}

pub fn wait_for_task_event(id: u64) -> Option<WaitEvent> {
    wait_inner(id, true, true)
}

/// First phase of a transactional `SYS_WAITPID`: block until `id` is a zombie
/// and return its exit status **without consuming the record**.
///
/// The target must be a child of the caller or covered by a target-scoped
/// `CAP_WAIT`; a foreign PID is reported as ECHILD exactly like a consumed
/// zombie, so existence does not leak.
///
/// The caller (the syscall layer) must hold the kernel lock on return, copy the
/// status to user memory, and then call [`consume_zombie`] in the same
/// non-blocking critical section. No `schedule`/BKL release may happen in
/// between; otherwise another waiter could consume the same zombie.
pub fn wait_status_for_pid(id: u64) -> Option<u64> {
    if id == IDLE_TASK_ID || id == current_task_id() || !current_task_can_wait(id) {
        return None;
    }
    match wait_inner(id, false, false) {
        Some(WaitEvent::Exited(status)) => Some(status),
        _ => None,
    }
}

/// Second phase of a transactional `SYS_WAITPID`: consume the zombie that was
/// peeked by [`wait_status_for_pid`].
///
/// Re-validates pid, state and status so a stale peek can never remove a
/// different task or report a mismatched status. Runs with interrupts masked and
/// the kernel lock held; it never blocks.
pub fn consume_zombie(id: u64, expected_status: u64) -> bool {
    // The transaction contract: peek and consume must be one BKL-protected
    // step so no other waiter can slip in between. This is a no-op in release.
    smp::bkl_assert_held();
    let saved_daif = interrupts::save_and_disable_irq();
    let consumed = scheduler_manager()
        .and_then(|manager| {
            let index = manager.tasks.iter().position(|task| {
                task.id == id
                    && task.state == TaskState::Zombie
                    && task.exit_status.unwrap_or(0) == expected_status
            })?;
            take_zombie(manager, index);
            Some(())
        })
        .is_some();
    interrupts::restore_irq(saved_daif);
    consumed
}

fn current_task_can_wait(id: u64) -> bool {
    let caller = current_task_id();
    if caller == id {
        return false;
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    if manager
        .tasks
        .iter()
        .any(|task| task.id == id && task.ppid == Some(caller))
    {
        return true;
    }
    manager
        .tasks
        .iter()
        .find(|task| task.id == caller)
        .map(|task| task.cap_table.has_target_rights(id, capability::CAP_WAIT))
        .unwrap_or(false)
}

fn wait_inner(id: u64, report_stopped: bool, consume: bool) -> Option<WaitEvent> {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return None;
    }
    let waiter = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    loop {
        let Some(manager) = scheduler_manager() else {
            interrupts::restore_irq(saved_daif);
            return None;
        };
        let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
            // Never existed, or the zombie record was already consumed here:
            // the caller maps this to ECHILD.
            interrupts::restore_irq(saved_daif);
            return None;
        };
        match manager.tasks[index].state {
            TaskState::Zombie => {
                if !consume {
                    // Transactional peek: report the status but keep the
                    // zombie available until the caller has copied it out.
                    let status = manager.tasks[index].exit_status.unwrap_or(0);
                    interrupts::restore_irq(saved_daif);
                    return Some(WaitEvent::Exited(status));
                }
                let status = take_zombie(manager, index);
                interrupts::restore_irq(saved_daif);
                return Some(WaitEvent::Exited(status));
            }
            TaskState::Stopped if report_stopped => {
                interrupts::restore_irq(saved_daif);
                return Some(WaitEvent::Stopped);
            }
            _ => {}
        }
        // Alive (including Dead-but-not-yet-reaped): block until the zombie
        // transition broadcasts on the process wait queue.
        PROCESS_WAIT_QUEUE.enqueue(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = Some(WaitLink::Immortal(&PROCESS_WAIT_QUEUE));
            task.state = TaskState::Blocked;
        }
        schedule_released();
        PROCESS_WAIT_QUEUE.remove(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = None;
        }
    }
}

/// Removes the zombie at `index` from the task table and returns its status.
///
/// Removal shifts every later index, so the current-task IDs of all online
/// CPUs are snapshotted first and remapped afterwards. Capability records that
/// target the dead task are dropped from the caller's table.
fn take_zombie(manager: &mut TaskManager, index: usize) -> u64 {
    let id = manager.tasks[index].id;
    debug_assert!(
        manager.tasks[index].running_on.is_none(),
        "removing a zombie that is still running on a CPU"
    );
    let status = manager.tasks[index].exit_status.unwrap_or(0);
    let cpu_ids = snapshot_current_ids(manager);
    let live_id = manager.tasks.get(smp::current_index()).map(|task| task.id);
    manager.tasks.remove(index);
    remap_current_indices(manager, &cpu_ids, live_id);
    // Every later task moved down one slot, so every queued index now names a
    // different task. Rebuild from the table instead of trying to shift.
    rebuild_ready_queues(manager);
    if let Some(current) = manager.tasks.get_mut(smp::current_index()) {
        current.cap_table.remove_for_target(id);
    }
    status
}

/// Moves every child of `dead_id` to the live reaper. Safe at any point: the
/// dead task may be a zombie already or only marked Dead.
fn reparent_children(manager: &mut TaskManager, dead_id: u64) {
    let reaper = reaper_pid(manager, dead_id);
    for task in &mut manager.tasks {
        if task.ppid == Some(dead_id) && task.id != dead_id {
            task.ppid = Some(reaper);
        }
    }
}

/// Chooses a live reaper without assuming PID 1 exists: the supervisor
/// (`silverd`) first, then any live user task, finally the always-present
/// idle task (0), which keeps orphans bounded via [`ZOMBIE_LIMIT`].
fn reaper_pid(manager: &TaskManager, excluded: u64) -> u64 {
    if let Some(task) = manager
        .tasks
        .iter()
        .filter(|task| {
            task.id != excluded
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program == user_program::SILVERD
        })
        .min_by_key(|task| task.id)
    {
        return task.id;
    }
    if let Some(task) = manager
        .tasks
        .iter()
        .filter(|task| {
            task.id != excluded
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program != u64::MAX
        })
        .min_by_key(|task| task.id)
    {
        return task.id;
    }
    IDLE_TASK_ID
}

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

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_remap_sentinel_selftest() -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(cpu) = (1..smp::MAX_CPUS)
        .find(|&cpu| smp::cpu_online(cpu) && smp::current_index_of(cpu) == usize::MAX)
    else {
        // Single CPU, or every AP already picked: nothing to prove here.
        return true;
    };

    // The probe must not shift `next_id`: user-space spawn logic assumes a
    // task id and its per-task cap-handle id advance in lockstep. Give the
    // probe an explicit id above the user space, like the churn tasks.
    const REMAP_PROBE_ID: u64 = 1 << 42;
    let saved_next = manager.next_id;
    manager.next_id = REMAP_PROBE_ID;
    let probe = spawn_kernel(remap_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        return false;
    };
    // Kill the probe before it can run, exactly like a task that died while
    // the AP was still parked, then force both remap paths: reap (no index
    // shift) and zombie removal (indices shift).
    manager.tasks[index].state = TaskState::Dead;
    manager.tasks[index].running_on = None;
    reap_dead_tasks(manager);
    let after_reap = smp::current_index_of(cpu);
    if let Some(zombie) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, zombie);
    }
    let after_remove = smp::current_index_of(cpu);
    let ok = after_reap == usize::MAX && after_remove == usize::MAX;
    if ok {
        console::write_str("SMP SELFTEST: remap sentinel PASS (AP");
        console::write_dec_u64(cpu as u64);
        console::write_str(" kept no-current-task)\r\n");
    } else {
        console::write_str("SMP SELFTEST: remap sentinel FAIL (cpu=");
        console::write_dec_u64(cpu as u64);
        console::write_str(" after_reap=");
        console::write_dec_u64(after_reap as u64);
        console::write_str(" after_remove=");
        console::write_dec_u64(after_remove as u64);
        console::write_str(")\r\n");
    }
    ok
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
    current_task_mut()?.change_program_break(requested)
}

/// Allocates `length` bytes of anonymous memory for the current task and maps
/// them into a kernel-chosen free virtual range with `prot` permissions.
pub fn mmap_anonymous(length: usize, prot: u64) -> Option<usize> {
    current_task_mut()?.mmap_anonymous(length, prot)
}

/// Unmaps an anonymous region previously returned by `mmap_anonymous`.
pub fn munmap(address: usize, length: usize) -> bool {
    current_task_mut()
        .map(|task| task.munmap(address, length))
        .unwrap_or(false)
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
    let boot_context = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) };

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

fn spawn_kernel(entry: extern "C" fn() -> !) -> Option<u64> {
    spawn_inner(Some(entry), None, Vec::new(), Vec::new(), Vec::new())
}

/// Spawns one idle task and marks it explicitly. Boot idle is id 0; AP idle
/// tasks are spawned from `finish_init` so they cannot shift SCOUT_ID or any
/// other spawn-order identity.
fn spawn_idle(explicit_id: Option<u64>) -> Option<u64> {
    let saved_next = {
        let manager = scheduler_manager()?;
        let saved = manager.next_id;
        if let Some(id) = explicit_id {
            manager.next_id = id;
        }
        saved
    };
    let result = spawn_kernel(idle_task);
    if let Some(manager) = scheduler_manager() {
        if explicit_id.is_some() {
            manager.next_id = saved_next;
        }
        if let Some(id) = result {
            if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
                task.is_idle = true;
            }
        }
    }
    result
}

pub fn spawn_user(program: u64, argument: u64) -> Option<u64> {
    spawn_inner(
        None,
        Some((program, argument)),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

pub fn spawn_user_with_cap(program: u64, argument: u64) -> Option<capability::CapId> {
    // The spawn handle needs one CapTable slot. Check before creating the task
    // so a full table cannot leak a running child without a handle.
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_user(program, argument)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Spawns a task from an already-loaded dynamic ELF image (the VFS/`run`
/// path). The image is consumed; on success the caller receives a capability
/// handle to the new task so it can lend it the console, wait on it and kill
/// it, exactly like [`spawn_user_with_cap`]. Unlike embedded programs, a
/// dynamically loaded task receives no kernel-granted capabilities by default
/// — any device rights must be delegated by its parent.
pub fn spawn_elf(image: LoadedImage, argument: u64) -> Option<capability::CapId> {
    spawn_elf_with_scope(image, argument, None)
}

/// [`spawn_elf`] with an optional write scope from the bundle manifest. The
/// task still receives no global rights: without `CAP_FS_WRITE` it may only
/// write paths under `fs_write_prefix`.
pub fn spawn_elf_with_scope(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
) -> Option<capability::CapId> {
    spawn_elf_with_policy(image, argument, fs_write_prefix, None)
}

/// [`spawn_elf_with_scope`] with an optional block/inode quota for the write
/// prefix. The quota is enforced by the syscall write paths; a trusted task
/// with its own `CAP_FS_WRITE` is never restricted by it.
pub fn spawn_elf_with_policy(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
) -> Option<capability::CapId> {
    spawn_elf_with_policy_and_args(
        image,
        argument,
        fs_write_prefix,
        fs_quota,
        Personality::Native,
        Vec::new(),
        Vec::new(),
    )
}

/// [`spawn_elf_with_policy`] with an explicit syscall personality and initial
/// `argv`/`envp`. A Linux image must pass at least `argv[0]`, because Linux
/// `_start` reads `argc` from the initial stack.
pub fn spawn_elf_with_policy_and_args(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    personality: Personality,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if scheduler_manager().map_or(true, |manager| !has_task_capacity(manager)) {
        // Dropping `image` releases its frames (RAII).
        return None;
    }
    // Reserve the parent's handle slot before the child exists; otherwise the
    // task would run without a capability and the insert would break the
    // CapTable bound.
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_elf_task(
        Some(image),
        None,
        argument,
        fs_write_prefix,
        fs_quota,
        personality,
        argv,
        envp,
        false,
    )?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Spawns a foreign (Linux ABI) image. Same capability model as
/// [`spawn_elf_with_policy_and_args`]: the child starts with no rights and the
/// parent gets a handle it can delegate.
pub fn spawn_linux_elf_with_args(
    image: LinuxImage,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if scheduler_manager().map_or(true, |manager| !has_task_capacity(manager)) {
        return None;
    }
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_elf_task(
        None,
        Some(image),
        0,
        fs_write_prefix,
        fs_quota,
        Personality::Linux,
        argv,
        envp,
        true,
    )?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

fn spawn_elf_task(
    user_image: Option<LoadedImage>,
    linux_image: Option<LinuxImage>,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    personality: Personality,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    foreground: bool,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    let id = manager.next_id;
    let cwd = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    let mut task = Task::new(
        id,
        None,
        Some((user_program::PROGRAM_DYNAMIC, argument)),
        user_image,
        linux_image,
        Vec::new(),
        argv,
        envp,
        current_task_pid(),
        fs_write_prefix,
        fs_quota,
        personality,
    )?;
    task.cwd = cwd;
    manager.tasks.push(task);
    manager.next_id += 1;
    // A freshly spawned task starts Ready: queue it on the spawning CPU.
    let index = manager.tasks.len() - 1;
    // A foreground child (the shell's `linuxrun`) gets the console before it
    // can be dispatched. Assigning it from the shell *after* spawn is racy: on
    // an SMP boot another CPU can run and even exit the child between the two
    // syscalls, and its output would be discarded.
    if foreground && current_task_owns_console() {
        set_console_owner(id);
    }
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
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
    let caller_id = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some((source_target, _)) =
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
        let shared_dma = manager.tasks.iter().any(|owner| {
            owner.dma_pages.iter().any(|&(base, page_count, _)| {
                base.checked_add(page_count * PAGE_SIZE)
                    .is_some_and(|dma_end| {
                        phys_addr >= base
                            && phys_addr
                                .checked_add(size)
                                .is_some_and(|requested_end| requested_end <= dma_end)
                    })
            })
        });
        let task = manager.tasks.get_mut(smp::current_index())?;
        let task_id = task.id;
        if !task
            .cap_table
            .has_self_rights(task_id, capability::CAP_MMIO)
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
            if !current
                .cap_table
                .has_self_rights(current.id, capability::CAP_IRQ)
            {
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

pub fn spawn_user_with_args_and_cap(
    program: u64,
    argument: u64,
    arguments: Vec<u8>,
) -> Option<capability::CapId> {
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_user_with_args(program, argument, arguments)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

pub fn spawn_user_with_args(program: u64, argument: u64, arguments: Vec<u8>) -> Option<u64> {
    spawn_inner(
        None,
        Some((program, argument)),
        arguments,
        Vec::new(),
        Vec::new(),
    )
}

/// Spawns an embedded program with `argv`/`envp` delivered on its initial
/// user stack (the versioned `SYS_SPAWN_ARGSV` path). The legacy flat
/// `arguments` blob stays empty, so `SYS_ARGS` keeps its old behaviour.
pub fn spawn_user_with_argv_envp_and_cap(
    program: u64,
    argument: u64,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_inner(None, Some((program, argument)), Vec::new(), argv, envp)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// PID of the caller for parent tracking. Boot-spawned tasks (no current
/// task yet, or the idle task) get `None` instead of a fake parent.
fn current_task_pid() -> Option<u64> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.id)
        .filter(|&id| id != IDLE_TASK_ID)
}

fn spawn_inner(
    entry: Option<extern "C" fn() -> !>,
    user: Option<(u64, u64)>,
    arguments: Vec<u8>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    let id = manager.next_id;

    // Resolve the embedded image before `Task::new`: a lookup or load failure
    // then needs no cleanup, because no frame has been allocated yet. The
    // dynamic path passes a pre-loaded image straight to `spawn_elf`.
    let image = match user {
        Some((program, _)) => {
            let bytes = user_program::image(program)?;
            Some(elf_loader::load(bytes)?)
        }
        None => None,
    };

    let ppid = current_task_pid();
    let cwd = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    // Wolfser persists its bookmarks in one root-level file. Give it a
    // path-scoped write window instead of the global CAP_FS_WRITE: the kernel
    // resolves the path and only `/sfs/wolfser.bookmarks` passes the check.
    let fs_write_prefix = user.and_then(|(program, _)| {
        if program == user_program::WOLFSER {
            Some(String::from("/sfs/wolfser.bookmarks"))
        } else {
            None
        }
    });
    let mut task = Task::new(
        id,
        entry,
        user,
        image,
        None,
        arguments,
        argv,
        envp,
        ppid,
        fs_write_prefix,
        None,
        Personality::Native,
    )?;
    task.cwd = cwd;

    manager.tasks.push(task);
    manager.next_id += 1;
    // A freshly spawned task starts Ready: queue it on the spawning CPU.
    let index = manager.tasks.len() - 1;
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
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
    let mut wake: Vec<u64> = Vec::new();
    for task in &mut manager.tasks {
        if task.state != TaskState::Dead || task.running_on.is_some() || task.resources_released {
            continue;
        }
        // Backstop unlink: ipc queues were already detached in `remove_task`
        // before their free, so a Dead task's pointer here is either None or
        // a `'static` queue (e.g. PROCESS_WAIT_QUEUE).
        detach_from_wait_queue(task);
        wake.extend(task.release_mutexes());
        task.cap_table.remove_all();
        task.bound_irq = None;
        task.irq_pending = false;
        task.release_stack();
        task.release_space(&dma_revokes);
        task.clear_resources();
        task.state = TaskState::Zombie;
        became_zombie.push(task.id);
    }
    if quiesced {
        smp::quiesce_end();
        // Parked CPUs resume in their SGI/BKL wait; kick them once so a task
        // made Ready during teardown is picked up even if its CPU returns to
        // user mode instead of scheduling immediately.
        smp::ipi_reschedule_others();
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

fn current_task_mut() -> Option<&'static mut Task> {
    let manager = scheduler_manager()?;
    // Bounds-checked borrow. The former `get_mut(index).map(|_| &mut
    // *tasks.add(index))` dance minted a second `&mut` alongside the one
    // `get_mut` had already produced — aliasing UB the optimizer is free to
    // exploit (observed as a self-loop in `receive_matching` once the
    // per-CPU index load changed the codegen).
    manager.tasks.get_mut(smp::current_index())
}

/// Self-capabilities a freshly spawned embedded program receives. Dynamic
/// ELF tasks start with none and must be delegated rights by their parent.
/// `CAP_MOUNT`/`CAP_POWER` are deliberately outside `CAP_ALL`, so a spawn
/// handle can never carry them.
fn initial_self_rights(program: u64) -> Option<u32> {
    let rights = match program {
        user_program::SILVERD => {
            capability::CAP_ALL | capability::CAP_MOUNT | capability::CAP_POWER
        }
        // The UART driver owns the input path, including the Ctrl-Z console
        // stop request; without this right SYS_STOP_CONSOLE is denied.
        user_program::HOWLER => {
            capability::CAP_MMIO | capability::CAP_IRQ | capability::CAP_CONSOLE_GRANT
        }
        user_program::NET_DRIVER => capability::CAP_MMIO | capability::CAP_PCI,
        user_program::NET_SERVER => capability::CAP_MMIO,
        // The shell hands device rights to the tasks it spawns by hand
        // (`spawn net_driver`), and `CapTable::grant` only allows a task to
        // pass on a right it already holds.
        // The shell needs MMIO only for the commands that map a device window.
        // In the console product build it is also the trusted control plane,
        // so the kernel grants it the power privilege directly; a spawn handle
        // can never delegate CAP_POWER (it is outside CAP_ALL).
        // The shell is the trusted control plane of the console OS; it owns
        // the power privilege directly (a spawn handle can never delegate it).
        user_program::HOWL => {
            capability::CAP_MMIO | capability::CAP_FS_WRITE | capability::CAP_POWER
        }
        // Trusted filesystem stress helper (not an installed app bundle).
        user_program::BIGDIR | user_program::BIGFILE => capability::CAP_FS_WRITE,
        // Multi-call Unix toolbox: `cp` writes, the rest only reads.
        user_program::UTILS => capability::CAP_FS_WRITE,
        // Wolfser scans PCI for ivshmem, maps that surface and the network
        // data window.
        user_program::WOLFSER => capability::CAP_MMIO | capability::CAP_PCI,
        _ => return None,
    };
    Some(rights)
}

/// Linux `auxv` entries for a loaded image.
///
/// `AT_RANDOM` and the terminating `AT_NULL` are appended by
/// `build_linux_stack`, so they are not listed here. `AT_PHDR` is zero when
/// the program headers are not covered by a `PT_LOAD`; static binaries that do
/// not consult it (musl's `_start`) are unaffected.
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
    let mut entries = Vec::new();
    entries.push((AT_PHDR, image.phdr_address as u64));
    entries.push((AT_PHENT, image.phentsize));
    entries.push((AT_PHNUM, image.phnum));
    entries.push((AT_PAGESZ, PAGE_SIZE as u64));
    entries.push((AT_BASE, 0));
    entries.push((AT_FLAGS, 0));
    entries.push((AT_ENTRY, image.entry as u64));
    entries.push((AT_UID, 0));
    entries.push((AT_EUID, 0));
    entries.push((AT_GID, 0));
    entries.push((AT_EGID, 0));
    entries.push((AT_SECURE, 0));
    entries
}

/// The subset of a loaded image that `auxv` needs, so the native and Linux
/// loaders share one builder.
#[derive(Copy, Clone)]
struct AuxvImage {
    entry: usize,
    phdr_address: usize,
    phentsize: u64,
    phnum: u64,
}

impl AuxvImage {
    fn native(image: &LoadedImage) -> Self {
        Self {
            entry: image.entry,
            phdr_address: image.phdr_address,
            phentsize: image.phentsize,
            phnum: image.phnum,
        }
    }

    fn linux(image: &LinuxImage) -> Self {
        Self {
            entry: image.entry,
            phdr_address: image.phdr_address,
            phentsize: image.phentsize,
            phnum: image.phnum,
        }
    }
}

impl Task {
    fn new(
        id: u64,
        entry: Option<extern "C" fn() -> !>,
        user: Option<(u64, u64)>,
        image: Option<LoadedImage>,
        linux_image: Option<LinuxImage>,
        arguments: Vec<u8>,
        argv: Vec<Vec<u8>>,
        envp: Vec<Vec<u8>>,
        ppid: Option<u64>,
        fs_write_prefix: Option<String>,
        fs_quota: Option<(u32, u32)>,
        personality: Personality,
    ) -> Option<Self> {
        // Build the argv/envp stack image before allocating anything: a
        // malformed or oversized argument list must fail without leaking.
        let argv_block = if argv.is_empty() && envp.is_empty() {
            None
        } else {
            if user.is_none() {
                return None;
            }
            // Linux wants the same argc/argv/envp prefix plus auxv. The values
            // are derivable from the loaded image, so a foreign ELF starts with
            // a stack its libc understands.
            let linux_auxv: Vec<(u64, u64)> = match personality {
                Personality::Linux => linux_image
                    .as_ref()
                    .map(|image| linux_auxv(AuxvImage::linux(image)))
                    .or_else(|| image.as_ref().map(|image| linux_auxv(AuxvImage::native(image))))
                    .unwrap_or_default(),
                Personality::Native => Vec::new(),
            };
            Some(match personality {
                Personality::Native => silver_microkernel::build_argv_envp(
                    &argv,
                    &envp,
                    mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
                    mmu::USER_STACK_SIZE - 1024,
                )?,
                Personality::Linux => silver_microkernel::build_linux_stack(
                    &argv,
                    &envp,
                    &linux_auxv,
                    mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
                    mmu::USER_STACK_SIZE - 1024,
                )?,
            })
        };
        // Charge the fixed footprint before allocating anything: the image
        // pages already exist (RAII releases them if this returns None), and
        // every other frame is allocated below. Page tables beyond the fixed
        // space are not charged separately (at most one L3 per 2 MiB).
        let image_frames = image.as_ref().map_or(0, |image| image.page_count)
            + linux_image.as_ref().map_or(0, |image| image.page_count());
        let ledger_frames = TASK_STACK_PAGES
            + mmu::SPACE_TABLE_FRAMES
            + 1 // private page
            + if user.is_some() {
                TASK_USER_STACK_PAGES + image_frames
            } else {
                0
            };
        if !mem_policy::task_charge_ok(0, ledger_frames, task_frame_limit()) {
            return None;
        }

        let Some(stack_start) = kernel_stack::alloc() else {
            return None;
        };

        let (user_stack, user_stack_generation) = if user.is_some() {
            let Some((user_start, generation)) =
                frame_allocator::alloc_user_contiguous_pages_tagged(TASK_USER_STACK_PAGES)
            else {
                kernel_stack::free(stack_start);
                return None;
            };
            (
                MemoryRegion::new(user_start, user_start + TASK_USER_STACK_SIZE),
                generation,
            )
        } else {
            (MemoryRegion::new(0, 0), 0)
        };

        let Some((private_page, private_page_generation)) = frame_allocator::alloc_user_page_tagged()
        else {
            if !user_stack.is_empty() {
                frame_allocator::free_contiguous_pages_tagged(
                    user_stack.start,
                    TASK_USER_STACK_PAGES,
                    user_stack_generation,
                );
            }
            kernel_stack::free(stack_start);
            return None;
        };

        let user_image = image;

        let Some((ttbr0, space_token)) =
            mmu::create_task_space(private_page, user_stack.start, user_image.as_ref())
        else {
            if let Some(image) = &user_image {
                image.release();
            }
            if let Some(image) = &linux_image {
                image.release();
            }
            frame_allocator::free_page_tagged(private_page, private_page_generation);
            if !user_stack.is_empty() {
                frame_allocator::free_contiguous_pages_tagged(
                    user_stack.start,
                    TASK_USER_STACK_PAGES,
                    user_stack_generation,
                );
            }
            kernel_stack::free(stack_start);
            return None;
        };

        // A Linux image is mapped at its own p_vaddr after the base space
        // exists; the loader cannot do it earlier because the space root is
        // created here. `destroy_task_space` also frees the private page.
        if let Some(image) = &linux_image {
            if !mmu::install_linux_image(ttbr0, image) {
                mmu::destroy_task_space(
                    space_token,
                    private_page,
                    private_page_generation,
                    mmu::asid_of(ttbr0),
                );
                if let Some(image) = &user_image {
                    image.release();
                }
                image.release();
                if !user_stack.is_empty() {
                    frame_allocator::free_contiguous_pages_tagged(
                        user_stack.start,
                        TASK_USER_STACK_PAGES,
                        user_stack_generation,
                    );
                }
                kernel_stack::free(stack_start);
                return None;
            }
        }

        // Publish the argv/envp block on the freshly mapped user stack before
        // the task can run. The stack VA range is backed by `user_stack`, so
        // the physical page behind a stack VA is `user_stack.start + offset`.
        let user_stack_pointer = if let Some(layout) = &argv_block {
            let offset = mmu::USER_STACK_SIZE - layout.block.len();
            let destination = phys_to_virt(user_stack.start + offset) as *mut u8;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    layout.block.as_ptr(),
                    destination,
                    layout.block.len(),
                );
            }
            layout.sp as u64
        } else if user.is_some() {
            (mmu::USER_STACK_VA + mmu::USER_STACK_SIZE) as u64
        } else {
            0
        };

        let stack = MemoryRegion::new(stack_start, stack_start + TASK_STACK_SIZE);
        let mut context = TaskContext::zero();
        context.registers[0] = if let Some(image) = &user_image {
            image.entry as u64
        } else if let Some(image) = &linux_image {
            image.entry as u64
        } else {
            entry.expect("kernel task entry is required") as usize as u64
        };
        context.registers[1] = user.map_or(0, |(_, argument)| argument);
        context.elr = if user.is_some() {
            silver_user_bootstrap as *const () as usize as u64
        } else {
            entry.expect("kernel task entry is required") as usize as u64
        };
        context.stack_pointer = phys_to_virt(stack.end) as u64;
        context.user_stack_pointer = user_stack_pointer;

        let mut cap_table = capability::CapTable::new();
        if let Some(rights) = user
            .map(|(program, _)| program)
            .and_then(initial_self_rights)
        {
            cap_table.insert(id, rights);
        }

        Some(Self {
            fs_write_prefix,
            fs_quota,
            id,
            state: TaskState::Ready,
            resume_state: None,
            is_idle: false,
            priority: sched_policy::PRIORITY_NORMAL,
            program: user.map_or(u64::MAX, |(program, _)| program),
            argument: user.map_or(0, |(_, argument)| argument),
            personality,
            cap_table,
            bound_irq: None,
            irq_pending: false,
            arguments,
            ppid,
            exit_status: None,
            resources_released: false,
            argv,
            envp,
            cwd: String::from("/"),
            owns_console: cfg!(feature = "demo-tasks"),
            context,
            stack,
            user_stack,
            user_stack_generation,
            private_page,
            private_page_generation,
            space_token,
            ttbr0,
            user_image,
            linux_image,
            heap_pages: Vec::new(),
            dma_pages: Vec::new(),
            mmap_regions: Vec::new(),
            mmap_bytes: 0,
            mem_frames: ledger_frames,
            program_break: mmu::USER_HEAP_VA,
            waiting_on: None,
            owned_mutexes: Vec::new(),
            running_on: None,
        })
    }

    /// Charges `count` frames to this task's ledger. All-or-nothing: the
    /// counter is untouched on refusal.
    fn charge_frames(&mut self, count: usize) -> bool {
        if !mem_policy::task_charge_ok(self.mem_frames, count, task_frame_limit()) {
            return false;
        }
        // `task_charge_ok` already proved `mem_frames + count` cannot overflow.
        self.mem_frames += count;
        true
    }

    fn release_frames(&mut self, count: usize) {
        self.mem_frames = self.mem_frames.saturating_sub(count);
    }

    fn change_program_break(&mut self, requested: usize) -> Option<usize> {
        if requested == 0 {
            return Some(self.program_break);
        }
        if (self.user_image.is_none() && self.linux_image.is_none())
            || requested < mmu::USER_HEAP_VA
            || requested > mmu::USER_HEAP_VA + mmu::USER_HEAP_MAX_SIZE
        {
            return None;
        }

        let target_pages = (requested - mmu::USER_HEAP_VA).div_ceil(PAGE_SIZE);
        let original_pages = self.heap_pages.len();
        if target_pages > original_pages {
            // Allocate the whole growth first and publish it as one batch:
            // hundreds of pages used to pay a full table clean plus
            // `dsb`/`tlbi` each. On any failure every freshly allocated frame
            // is returned and the address space is untouched.
            let growth = target_pages - original_pages;
            if !self.charge_frames(growth) {
                return None;
            }
            let mut frames: Vec<usize> = Vec::with_capacity(growth);
            let mut generations: Vec<u32> = Vec::with_capacity(growth);
            for _ in 0..growth {
                let Some((frame, generation)) = frame_allocator::alloc_user_page_tagged() else {
                    for (&frame, &generation) in frames.iter().zip(generations.iter()) {
                        frame_allocator::free_page_tagged(frame, generation);
                    }
                    self.release_frames(growth);
                    return None;
                };
                frames.push(frame);
                generations.push(generation);
            }
            if !mmu::map_user_heap_pages(self.ttbr0, original_pages, &frames) {
                for (&frame, &generation) in frames.iter().zip(generations.iter()) {
                    frame_allocator::free_page_tagged(frame, generation);
                }
                self.release_frames(growth);
                return None;
            }
            self.heap_pages
                .extend(frames.into_iter().zip(generations.into_iter()));
        }

        self.truncate_heap(target_pages);
        self.program_break = requested;
        Some(requested)
    }

    fn truncate_heap(&mut self, target_pages: usize) {
        let before = self.heap_pages.len();
        while self.heap_pages.len() > target_pages {
            let index = self.heap_pages.len() - 1;
            let mapped = mmu::unmap_user_heap_page(self.ttbr0, index)
                .expect("tracked user heap mapping is missing");
            let tracked = self.heap_pages.pop().unwrap();
            debug_assert_eq!(mapped, tracked.0);
            frame_allocator::free_page_tagged(mapped, tracked.1);
        }
        self.release_frames(before.saturating_sub(self.heap_pages.len()));
    }

    /// Allocates and maps `length` bytes of anonymous memory into a free virtual
    /// range in the task's TTBR0, returning the chosen address.
    fn mmap_anonymous(&mut self, length: usize, prot: u64) -> Option<usize> {
        // W^X: a single anonymous mapping must never be writable and
        // executable at the same time. Validate before placement, frame
        // allocation, PTE publication or `mmap_regions` bookkeeping, so a
        // rejected request leaves no state behind.
        if prot & mmu::PROT_WRITE != 0 && prot & mmu::PROT_EXEC != 0 {
            return None;
        }
        if self.mmap_regions.len() >= MAX_MMAP_REGIONS {
            return None;
        }
        // `length` is user-controlled: the pure placement helper bounds the
        // page count against the window and uses checked arithmetic, so an
        // absurd request (e.g. `usize::MAX`) returns None instead of
        // overflowing `page_count * PAGE_SIZE` and panicking the kernel.
        let (virtual_address, page_count) = silver_microkernel::mmap_place(
            length,
            mmu::USER_MMAP_VA,
            mmu::USER_MMAP_MAX_VA,
            PAGE_SIZE,
            self.mmap_regions
                .iter()
                .map(|region| (region.virtual_address, region.page_count)),
        )?;

        // Per-task memory quota: reject before any frame or PTE is touched, so
        // a refused request leaves no state behind.
        let span = page_count.checked_mul(PAGE_SIZE)?;
        if !silver_microkernel::mmap_quota_ok(self.mmap_bytes, span, MAX_MMAP_BYTES_PER_TASK) {
            return None;
        }

        // Charge the data pages before the frame allocation. Page tables added
        // by `map_user_pages_at` beyond the fixed space cost at most one L3 per
        // 2 MiB (0.2%), which the flat `SPACE_TABLE_FRAMES` charge covers for
        // budgeting purposes.
        if !self.charge_frames(page_count) {
            return None;
        }
        let (physical_start, generation) =
            match frame_allocator::alloc_user_contiguous_pages_tagged(page_count) {
                Some(pair) => pair,
                None => {
                    self.release_frames(page_count);
                    return None;
                }
            };
        if !mmu::map_user_pages_at(
            self.ttbr0,
            virtual_address,
            physical_start,
            page_count,
            prot,
        ) {
            frame_allocator::free_contiguous_pages_tagged(physical_start, page_count, generation);
            self.release_frames(page_count);
            return None;
        }

        let region = MmapRegion {
            virtual_address,
            page_count,
            physical_start,
            generation,
        };
        let position = self
            .mmap_regions
            .iter()
            .position(|existing| existing.virtual_address > virtual_address)
            .unwrap_or(self.mmap_regions.len());
        self.mmap_regions.insert(position, region);
        self.mmap_bytes += span;
        Some(virtual_address)
    }

    /// Unmaps a page-aligned sub-range of one anonymous mapping. A range in
    /// the middle of a mapping splits the bookkeeping into two regions; the
    /// pages on both sides keep the original physical run and generation.
    fn munmap(&mut self, address: usize, length: usize) -> bool {
        let page_count = length.div_ceil(PAGE_SIZE);
        // Same user-controlled bound as `mmap_anonymous`: an absurd length
        // must be rejected up front, before the unmap loop walks pages.
        if page_count == 0 || page_count > mmu::USER_MMAP_MAX_PAGES {
            return false;
        }
        let Some(removal) = silver_microkernel::mmap_remove(
            self.mmap_regions
                .iter()
                .map(|region| (region.virtual_address, region.page_count)),
            address,
            page_count,
            PAGE_SIZE,
        ) else {
            return false;
        };
        // A middle range needs one extra bookkeeping slot. Refuse before any
        // PTE or frame is touched, so a full table leaves the mapping intact.
        if removal.left_pages > 0
            && removal.right_pages > 0
            && self.mmap_regions.len() >= MAX_MMAP_REGIONS
        {
            return false;
        }

        let region = &self.mmap_regions[removal.index];
        let (virtual_address, physical_start, generation) = (
            region.virtual_address,
            region.physical_start,
            region.generation,
        );
        let span = page_count * PAGE_SIZE;
        let removed_physical = physical_start + (address - virtual_address);

        // Fail closed: `unmap_user_pages_at` is all-or-nothing, so on failure
        // nothing was unmapped and the region stays tracked. Freeing the run
        // here would leave live PTEs pointing at reclaimed frames.
        if !mmu::unmap_user_pages_at(self.ttbr0, address, page_count) {
            MMAP_LEAKED_FRAMES.fetch_add(page_count, Ordering::Relaxed);
            console::write_str("MMAP LEAK: range 0x");
            console::write_hex_u64(removed_physical as u64);
            console::write_str(" left mapped after refused unmap (");
            console::write_dec_u64(page_count as u64);
            console::write_str(" pages)\r\n");
            return false;
        }
        // Free only the removed slice of the original contiguous run; every
        // page in it shares the run's generation.
        if !frame_allocator::free_contiguous_pages_tagged(removed_physical, page_count, generation) {
            console::write_str("MMAP: allocator refused run 0x");
            console::write_hex_u64(removed_physical as u64);
            console::write_str(" - leaked\r\n");
        }

        let left = removal.left_pages;
        let right = removal.right_pages;
        match (left > 0, right > 0) {
            (false, false) => {
                self.mmap_regions.remove(removal.index);
            }
            (true, false) => {
                self.mmap_regions[removal.index].page_count = left;
            }
            (false, true) => {
                let region = &mut self.mmap_regions[removal.index];
                region.virtual_address = address + span;
                region.physical_start = removed_physical + span;
                region.page_count = right;
            }
            (true, true) => {
                let tail = MmapRegion {
                    virtual_address: address + span,
                    physical_start: removed_physical + span,
                    page_count: right,
                    generation,
                };
                self.mmap_regions[removal.index].page_count = left;
                self.mmap_regions.insert(removal.index + 1, tail);
            }
        }
        self.mmap_bytes = self.mmap_bytes.saturating_sub(span);
        self.release_frames(page_count);
        true
    }

    fn release_mutexes(&mut self) -> Vec<u64> {
        let mut wake = Vec::new();
        for state in self.owned_mutexes.drain(..) {
            if let Some(waiter) = unsafe { mutex::release_owner(state, self.id) } {
                wake.push(waiter);
            }
        }
        wake
    }

    fn release_stack(&self) {
        if self.resources_released {
            return;
        }
        if !kernel_stack::free(self.stack.start) {
            console::write_str("STACK: kernel stack release refused for 0x");
            console::write_hex_u64(self.stack.start as u64);
            console::write_str(" - leaked\r\n");
        }
        if !self.user_stack.is_empty()
            && !frame_allocator::free_contiguous_pages_tagged(
                self.user_stack.start,
                TASK_USER_STACK_PAGES,
                self.user_stack_generation,
            )
        {
            console::write_str("STACK: user stack release refused for 0x");
            console::write_hex_u64(self.user_stack.start as u64);
            console::write_str(" - leaked\r\n");
        }
    }

    /// Tears the task's address space down. `dma_revokes` carries the revoke
    /// results collected in `reap_dead_tasks` (before any teardown): a range
    /// is released only when its scan proved `safe_to_free`; otherwise the
    /// frames are deliberately leaked (see `DMA_LEAKED_FRAMES`) rather than
    /// handed back while a foreign mapping might still reference them.
    ///
    /// Quiescence precondition: `safe_to_free` is a CPU-mapping proof. It does
    /// not prove that the device stopped DMA. The kernel therefore hands a
    /// provably-unmapped range to the DMA quarantine (see
    /// [`DMA_QUARANTINE_EPOCHS`]) instead of freeing it at once, which gives
    /// an armed queue a bounded drain window; a driver that owns a device must
    /// still stop the queue (or reset it and observe completion) before it
    /// exits, and a full quarantine falls back to the leak policy.
    ///
    /// Guarded by `resources_released`/`ttbr0`: a zombie has already run this
    /// once, and calling it again with `ttbr0 == 0` would dereference zero.
    fn release_space(&mut self, dma_revokes: &[(usize, usize, mmu::RevokeResult)]) {
        if self.resources_released || self.ttbr0 == 0 {
            return;
        }
        for region in &self.mmap_regions {
            if mmu::unmap_user_pages_at(self.ttbr0, region.virtual_address, region.page_count) {
                frame_allocator::free_contiguous_pages_tagged(
                    region.physical_start,
                    region.page_count,
                    region.generation,
                );
            } else {
                // A live PTE still references these frames; leaking is the
                // only safe outcome.
                MMAP_LEAKED_FRAMES.fetch_add(region.page_count, Ordering::Relaxed);
                console::write_str("MMAP LEAK: teardown could not unmap range 0x");
                console::write_hex_u64(region.physical_start as u64);
                console::write_str(" (");
                console::write_dec_u64(region.page_count as u64);
                console::write_str(" pages)\r\n");
            }
        }
        // Heap data pages carry their own tags; the address-space teardown
        // frees only the page tables and the private page.
        for (page, generation) in self.heap_pages.drain(..) {
            if !frame_allocator::free_page_tagged(page, generation) {
                console::write_str("HEAP: release refused for page 0x");
                console::write_hex_u64(page as u64);
                console::write_str(" - leaked\r\n");
            }
        }
        if !mmu::destroy_task_space(
            self.space_token,
            self.private_page,
            self.private_page_generation,
            mmu::asid_of(self.ttbr0),
        ) {
            console::write_str("MMU: address space teardown was refused\r\n");
        }
        for &(physical, page_count, generation) in &self.dma_pages {
            // Exact (physical, page_count) match: allocator ranges never
            // overlap, so a dead task's own range cannot be confused with
            // another dead task's range in the same reap pass.
            let record = dma_revokes
                .iter()
                .find(|(base, pages, _)| *base == physical && *pages == page_count);
            match record {
                Some((_, _, result)) if result.safe_to_free => {
                    // Quarantine instead of freeing immediately: a device
                    // queue may still be draining even though every CPU
                    // mapping is gone. See `DMA_QUARANTINE_EPOCHS`.
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
        if let Some(image) = &self.user_image {
            image.release();
        }
        if let Some(image) = &self.linux_image {
            image.release();
        }
    }

    /// Drops the last executable-resource references after teardown. The
    /// zombie keeps only what `wait` needs (PID, PPID, exit status); clearing
    /// the fields makes a second teardown a no-op (see the guards above).
    fn clear_resources(&mut self) {
        self.stack = MemoryRegion::new(0, 0);
        self.user_stack = MemoryRegion::new(0, 0);
        self.user_stack_generation = 0;
        self.private_page = 0;
        self.private_page_generation = 0;
        self.space_token = TableToken::empty();
        self.ttbr0 = 0;
        self.user_image = None;
        self.linux_image = None;
        self.heap_pages.clear();
        self.dma_pages.clear();
        self.mmap_regions.clear();
        self.mmap_bytes = 0;
        // The ledger is only meaningful while the task can allocate; a zombie
        // keeps metadata only, so drop the stale charge with the resources.
        self.mem_frames = 0;
        self.arguments = Vec::new();
        self.argv.clear();
        self.envp.clear();
        self.cwd = String::from("/");
        self.resume_state = None;
        self.waiting_on = None;
        self.context = TaskContext::zero();
        self.resources_released = true;
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
