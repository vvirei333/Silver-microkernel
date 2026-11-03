use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::sync::Arc;
use alloc::{string::String, vec::Vec};

use silver_microkernel::table_registry::TableToken;

use crate::{
    capability,
    condvar::Condvar,
    elf::LoadedImage,
    elf_loader,
    frame_allocator, interrupts, smp,
    memory::{phys_to_virt, virt_to_phys, MemoryRegion, PAGE_SIZE},
    mmu, mutex, console, timer, user_program, vfs, kernel_stack,
    wait_queue::WaitQueue,
};

const TASK_STACK_PAGES: usize = 4;
const TASK_STACK_SIZE: usize = TASK_STACK_PAGES * PAGE_SIZE;
const TASK_USER_STACK_PAGES: usize = mmu::USER_STACK_PAGES;
const TASK_USER_STACK_SIZE: usize = mmu::USER_STACK_SIZE;
const IDLE_TASK_ID: u64 = 0;

/// AP idle-task ids live above the user id space so spawning them cannot
/// shift SCOUT_ID or any other spawn-order identity.
const AP_IDLE_ID_BASE: u64 = 1 << 40;

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

pub struct Task {
    pub id: u64,
    pub state: TaskState,
    resume_state: Option<TaskState>,
    pub program: u64,
    pub argument: u64,
    pub cap_table: capability::CapTable,
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
    pub context: TaskContext,
    pub stack: MemoryRegion,
    pub user_stack: MemoryRegion,
    pub ttbr0: usize,
    user_image: Option<LoadedImage>,
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

pub struct TaskManager {
    tasks: Vec<Task>,
    next_id: u64,
    console_parent: Option<u64>,
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

/// Set once `init` has published the first task and `set_current_index`.
///
/// Until then there is no valid current task: a timer IRQ can fire inside
/// `scheduler::init` because the PL011 TX wait temporarily unmasks IRQs, and
/// the clamp in `schedule` would mistake idle for the running task and save the
/// boot stack into idle's context. `schedule` must be a no-op before this flag.
static SCHEDULER_READY: AtomicBool = AtomicBool::new(false);

/// Data frames of shared-DMA ranges whose revoke scan could not prove the
/// absence of foreign mappings (see `reap_dead_tasks`): deliberately leaked
/// instead of risking a use-after-free through a live foreign mapping.
static DMA_LEAKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// mmap runs that could not be unmapped completely: the backing frames are
/// leaked instead of freed, because a still-live PTE must never point at a
/// frame the allocator can hand out again.
static MMAP_LEAKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

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
    });

    let _idle_id = spawn_idle(None);

    #[cfg(not(feature = "demo-tasks"))]
    {
        let silverd_id =
            spawn_user(user_program::SILVERD, 0).expect("silverd must be created at boot");
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
    });

    let _idle_id = spawn_idle(None);
    let _scout_id = spawn_user(user_program::SCOUT, 0).expect("scout must start on fog");
    let howl_id = spawn_user(user_program::HOWL, 0).expect("howl must start on fog");
    let _ = set_console_owner(howl_id);

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
    smp::set_current_index(
        manager
            .tasks
            .iter()
            .position(|task| task.id == first_id)
            .unwrap_or(0),
    );
    // From here on `schedule` may run: the current index names a real task.
    SCHEDULER_READY.store(true, Ordering::Release);
}

pub fn yield_current() {
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        if task.state == TaskState::Running {
            task.state = TaskState::Ready;
        }
    }
   
    schedule_released();
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
    schedule_released();
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
            manager
                .tasks
                .iter()
                .any(|task| !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq == Some(intid))
        })
        .unwrap_or(false)
}

pub fn wake_bound_irq(intid: u32) -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(task) = manager
        .tasks
        .iter_mut()
        .find(|task| !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq == Some(intid))
    else {
        return false;
    };
    task.irq_pending = true;
    wake_blocked_task(task);
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
                !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq == Some(crate::gic::uart_irq())
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
            manager
                .tasks
                .iter_mut()
                .find(|task| {
                    task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq.is_some()
                })
                .map(|task| {
                    task.irq_pending = true;
                    wake_blocked_task(task);
                })
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
        if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
            wake_blocked_task(task);
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
    console::write_str("EXIT: task=");
    console::write_dec_u64(id);
    console::write_str(" program=");
    console::write_dec_u64(program);
    console::write_str(" status=");
    console::write_dec_u64(status);
    // Packed reports (see the filesystem server's mount failure) are unreadable
    // in decimal, so the same value is repeated in hex on the same line.
    console::write_str(" (0x");
    console::write_hex_u64(status);
    console::write_str(")\r\n");
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

fn snapshot_current_ids(manager: &TaskManager) -> Vec<Option<u64>> {
    (0..smp::MAX_CPUS)
        .filter(|&cpu| smp::cpu_online(cpu))
        .map(|cpu| {
            manager
                .tasks
                .get(smp::current_index_of(cpu))
                .map(|task| task.id)
        })
        .collect()
}

fn remap_current_indices(manager: &TaskManager, snapshot: &[Option<u64>], live_id: Option<u64>) {
    for (cpu, current_id) in (0..smp::MAX_CPUS)
        .filter(|&cpu| smp::cpu_online(cpu))
        .zip(snapshot)
    {
        // Preserve the "no current task" sentinel: an AP that has not picked
        // a task yet must not be remapped to idle after an unrelated task is
        // removed. A removed current id also leaves the CPU with no task.
        let index = current_id
            .and_then(|id| manager.tasks.iter().position(|task| task.id == id))
            .unwrap_or(usize::MAX);
        smp::set_current_index_for(cpu, index);
    }
    smp::set_current_index(
        live_id
            .and_then(|id| manager.tasks.iter().position(|task| task.id == id))
            .unwrap_or(usize::MAX),
    );
}

fn wake_process_waiters(manager: &mut TaskManager) {
    let mut woke_any = false;
    while let Some(waiter) = PROCESS_WAIT_QUEUE.pop() {
        if let Some(task) = manager
            .tasks
            .iter_mut()
            .find(|task| {
                task.id == waiter
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            })
        {
            wake_blocked_task(task);
            woke_any = true;
        }
    }
    if woke_any {
        smp::ipi_reschedule_others();
    }
}

fn wake_blocked_task(task: &mut Task) {
    match task.state {
        TaskState::Blocked | TaskState::BlockedUntil(_) => {
            task.state = TaskState::Ready;
            task.waiting_on = None;
        }
        TaskState::Stopped
            if matches!(
                task.resume_state,
                Some(TaskState::Blocked | TaskState::BlockedUntil(_))
            ) =>
        {
            task.resume_state = Some(TaskState::Ready);
            task.waiting_on = None;
        }
        _ => {}
    }
}

pub fn current_task_id() -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(smp::current_index()))
        .map(|task| task.id)
        .unwrap_or(0)
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
            && task.ttbr0 == ttbr0
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
            if let Some(task) = manager
                .tasks
                .iter_mut()
                .find(|task| task.owns_console && !matches!(task.state, TaskState::Dead | TaskState::Zombie))
            {
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
            let Some(task) = manager
                .tasks
                .iter_mut()
                .find(|task| task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie))
            else {
                return false;
            };
            if task.state == TaskState::Stopped {
                task.state = task.resume_state.take().unwrap_or(TaskState::Ready);
                if task.state == TaskState::Running {
                    task.state = TaskState::Ready;
                }
            }
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
                && manager
                    .tasks
                    .iter()
                    .any(|task| task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie));
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
            manager
                .tasks
                .iter()
                .any(|task| task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie))
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
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFFFF_0000); // red: scheduler::start entered
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
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFFFF_8000); // orange: BKL acquired
    first_task.state = TaskState::Running;
    first_task.running_on = Some(0);
    trace_switch(
        0,
        first_task.id,
        first_task.context.stack_pointer,
        first_task.context.elr,
    );
    let first_context = &first_task.context as *const TaskContext;
    let boot_context = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) };

    mmu::switch_address_space(first_task.ttbr0);
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFFFF_FF00); // yellow: TTBR0 switched
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFF00_FFFF); // cyan: about to context-switch

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
            smp::bkl_release();
            return;
        }
    }
    if had_current && manager.tasks[current_index].state == TaskState::Running {
        manager.tasks[current_index].state = TaskState::Ready;
    }

    let scan_base = if had_current { current_index } else { 0 };
    let Some(next_index) = find_next_task(manager, scan_base) else {
        if had_current && manager.tasks[current_index].state == TaskState::Ready {
            manager.tasks[current_index].state = TaskState::Running;
        }
        smp::bkl_release();
        return;
    };
    if had_current && next_index == current_index && !live_is_dead {
        // Self-continuation: this CPU keeps running the same task and keeps
        // holding the kernel lock for it. The probe above marked it Ready so
        // `find_next_task` could consider it; restore Running before another
        // CPU can mistake it for runnable.
        manager.tasks[current_index].state = TaskState::Running;
        smp::bkl_release();
        return;
    }

    // Switching away: the outgoing task stops running on this CPU. An AP's
    // first pass has no outgoing task, so there is no state to clear.
    if had_current {
        match manager.tasks.get_mut(current_index) {
            Some(outgoing) => outgoing.running_on = None,
            None => {
                console::write_str("SCHEDULE: current_index out of range: ");
                console::write_dec_u64(current_index as u64);
                console::write_str("\r\n");
            }
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
    smp::set_current_index(next_index);

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
    let task_id = spawn_elf_task(image, argument)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

fn spawn_elf_task(image: LoadedImage, argument: u64) -> Option<u64> {
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
        Some(image),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        current_task_pid(),
    )?;
    task.cwd = cwd;
    manager.tasks.push(task);
    manager.next_id += 1;
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

pub fn map_phys_to_current(phys_addr: usize, size: usize) -> Option<usize> {
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
        if !task.cap_table.has_self_rights(task_id, capability::CAP_MMIO) {
            return None;
        }
        // Capacity is checked before the PTE is installed. The operation is
        // all-or-nothing: a failed bookkeeping step must not leave an MMIO
        // mapping that no capability authorises (the old order mapped first
        // and silently skipped the insert when the table was full).
        if !task.cap_table.has_capacity() {
            return None;
        }
        let virtual_address = if shared_dma {
            mmu::map_shared_dma_to_user(task, phys_addr, size)?
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

pub fn alloc_dma_for_current(size: usize) -> Option<(usize, usize)> {
    if size == 0 || size > mmu::USER_DMA_MAX_SIZE {
        return None;
    }
    let page_count = size.div_ceil(PAGE_SIZE);
    let mapped_size = page_count * PAGE_SIZE;
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let task = manager.tasks.get_mut(smp::current_index())?;
        let task_id = task.id;
        if !task.cap_table.has_self_rights(task_id, capability::CAP_MMIO) {
            console::write_str("DMA: FAIL - no CAP_MMIO rights\r\n");
            return None;
        }
        let (physical, generation) =
            match frame_allocator::alloc_contiguous_pages_tagged(page_count) {
                Some(pair) => pair,
                None => {
                    console::write_str("DMA: FAIL - alloc_contiguous_pages returned None\r\n");
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
            return None;
        };
        task.dma_pages.push((physical, page_count, generation));
        Some((virtual_address, physical))
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
    let mut task = Task::new(id, entry, user, image, arguments, argv, envp, ppid)?;
    task.cwd = cwd;

    manager.tasks.push(task);
    manager.next_id += 1;
    Some(id)
}

fn find_next_task(manager: &TaskManager, scan_base: usize) -> Option<usize> {
    let task_count = manager.tasks.len();

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if !task.is_idle
            && task.state == TaskState::Ready
            && task.running_on.is_none()
        {
            return Some(index);
        }
    }

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if task.state == TaskState::Ready && task.running_on.is_none() {
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
    for task in &mut manager.tasks {
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
        }
    }
}

fn reap_dead_tasks(manager: &mut TaskManager) {
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
        task.state == TaskState::Dead
            && task.running_on.is_none()
            && !task.dma_pages.is_empty()
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
            console::write_str(if aggregated.safe_to_free { "true" } else { "false" });
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
        if task.state != TaskState::Dead
            || task.running_on.is_some()
            || task.resources_released
        {
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
        if let Some(task) = manager.tasks.iter_mut().find(|task| {
            task.id == id && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
        }) {
            wake_blocked_task(task);
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

impl Task {
    fn new(
        id: u64,
        entry: Option<extern "C" fn() -> !>,
        user: Option<(u64, u64)>,
        image: Option<LoadedImage>,
        arguments: Vec<u8>,
        argv: Vec<Vec<u8>>,
        envp: Vec<Vec<u8>>,
        ppid: Option<u64>,
    ) -> Option<Self> {
        // Build the argv/envp stack image before allocating anything: a
        // malformed or oversized argument list must fail without leaking.
        let argv_block = if argv.is_empty() && envp.is_empty() {
            None
        } else {
            if user.is_none() {
                return None;
            }
            Some(silver_microkernel::build_argv_envp(
                &argv,
                &envp,
                mmu::USER_STACK_VA + mmu::USER_STACK_SIZE,
                mmu::USER_STACK_SIZE - 1024,
            )?)
        };
        let Some(stack_start) = kernel_stack::alloc() else {
            return None;
        };

        let (user_stack, user_stack_generation) = if user.is_some() {
            let Some((user_start, generation)) =
                frame_allocator::alloc_contiguous_pages_tagged(TASK_USER_STACK_PAGES)
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

        let Some((private_page, private_page_generation)) = frame_allocator::alloc_page_tagged()
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
        match user.map(|(program, _)| program) {
            Some(user_program::SILVERD) => {
                // System manager: full task/device rights plus the global
                // mount and power rights. CAP_MOUNT/CAP_POWER are deliberately
                // NOT part of CAP_ALL, so spawn handles can never carry them.
                cap_table.insert(
                    id,
                    capability::CAP_ALL | capability::CAP_MOUNT | capability::CAP_POWER,
                );
            }
            Some(user_program::HOWLER) => {
                cap_table.insert(id, capability::CAP_MMIO | capability::CAP_IRQ);
            }
            Some(user_program::BLOCK_DRIVER) => {
                cap_table.insert(id, capability::CAP_MMIO | capability::CAP_PCI);
            }
            Some(user_program::NET_DRIVER) => {
                cap_table.insert(id, capability::CAP_MMIO | capability::CAP_PCI);
            }
            // The filesystem server only needs MMIO to map the shared DMA
            // window the block driver hands back; without this a server spawned
            // by hand from the shell could never mount.
            Some(user_program::SFS_SERVER) => {
                cap_table.insert(id, capability::CAP_MMIO);
            }
            // The network server allocates its own DMA window and may map the
            // driver's buffers as the protocol grows.
            Some(user_program::NET_SERVER) => {
                cap_table.insert(id, capability::CAP_MMIO);
            }
            // The shell hands device rights to the tasks it spawns by hand
            // (`spawn block_driver`), and `CapTable::grant` only allows a task
            // to pass on a right it already holds. Without this entry every
            // delegation from the shell would fail.
            Some(user_program::HOWL) => {
                cap_table.insert(id, capability::CAP_MMIO);
            }
            // The graphics driver owns the on-screen Power button; only this
            // trusted component receives the power privilege.
            Some(user_program::GPU_DRIVER) => {
                cap_table.insert(id, capability::CAP_MMIO | capability::CAP_POWER);
            }
            _ => {}
        }

        Some(Self {
            id,
            state: TaskState::Ready,
            resume_state: None,
            is_idle: false,
            program: user.map_or(u64::MAX, |(program, _)| program),
            argument: user.map_or(0, |(_, argument)| argument),
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
            heap_pages: Vec::new(),
            dma_pages: Vec::new(),
            mmap_regions: Vec::new(),
            program_break: mmu::USER_HEAP_VA,
            waiting_on: None,
            owned_mutexes: Vec::new(),
            running_on: None,
        })
    }

    fn change_program_break(&mut self, requested: usize) -> Option<usize> {
        if requested == 0 {
            return Some(self.program_break);
        }
        if self.user_image.is_none()
            || requested < mmu::USER_HEAP_VA
            || requested > mmu::USER_HEAP_VA + mmu::USER_HEAP_MAX_SIZE
        {
            return None;
        }

        let target_pages = (requested - mmu::USER_HEAP_VA).div_ceil(PAGE_SIZE);
        let original_pages = self.heap_pages.len();
        while self.heap_pages.len() < target_pages {
            let index = self.heap_pages.len();
            let Some((frame, generation)) = frame_allocator::alloc_page_tagged() else {
                self.truncate_heap(original_pages);
                return None;
            };
            if !mmu::map_user_heap_page(self.ttbr0, index, frame) {
                frame_allocator::free_page_tagged(frame, generation);
                self.truncate_heap(original_pages);
                return None;
            }
            self.heap_pages.push((frame, generation));
        }

        self.truncate_heap(target_pages);
        self.program_break = requested;
        Some(requested)
    }

    fn truncate_heap(&mut self, target_pages: usize) {
        while self.heap_pages.len() > target_pages {
            let index = self.heap_pages.len() - 1;
            let mapped = mmu::unmap_user_heap_page(self.ttbr0, index)
                .expect("tracked user heap mapping is missing");
            let tracked = self.heap_pages.pop().unwrap();
            debug_assert_eq!(mapped, tracked.0);
            frame_allocator::free_page_tagged(mapped, tracked.1);
        }
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

        let (physical_start, generation) =
            frame_allocator::alloc_contiguous_pages_tagged(page_count)?;
        if !mmu::map_user_pages_at(self.ttbr0, virtual_address, physical_start, page_count, prot) {
            frame_allocator::free_contiguous_pages_tagged(
                physical_start,
                page_count,
                generation,
            );
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
        Some(virtual_address)
    }

    /// Unmaps the anonymous region that exactly matches `address` and `length`.
    fn munmap(&mut self, address: usize, length: usize) -> bool {
        let page_count = length.div_ceil(PAGE_SIZE);
        // Same user-controlled bound as `mmap_anonymous`: an absurd length
        // must be rejected up front, before the unmap loop walks pages.
        if page_count == 0 || page_count > mmu::USER_MMAP_MAX_PAGES {
            return false;
        }
        let Some(position) = self.mmap_regions.iter().position(|region| {
            region.virtual_address == address && region.page_count == page_count
        }) else {
            return false;
        };
        let region = &self.mmap_regions[position];
        let (virtual_address, physical_start, generation) = (
            region.virtual_address,
            region.physical_start,
            region.generation,
        );
        // Fail closed: `unmap_user_pages_at` is all-or-nothing, so on failure
        // nothing was unmapped and the region stays tracked. Freeing the run
        // here would leave live PTEs pointing at reclaimed frames.
        if !mmu::unmap_user_pages_at(self.ttbr0, virtual_address, page_count) {
            MMAP_LEAKED_FRAMES.fetch_add(page_count, Ordering::Relaxed);
            console::write_str("MMAP LEAK: range 0x");
            console::write_hex_u64(physical_start as u64);
            console::write_str(" left mapped after refused unmap (");
            console::write_dec_u64(page_count as u64);
            console::write_str(" pages)\r\n");
            return false;
        }
        self.mmap_regions.remove(position);
        if !frame_allocator::free_contiguous_pages_tagged(physical_start, page_count, generation) {
            console::write_str("MMAP: allocator refused run 0x");
            console::write_hex_u64(physical_start as u64);
            console::write_str(" - leaked\r\n");
        }
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
    /// not prove that the device stopped DMA. Automatic reclamation here is
    /// sound only while the platform has no IOMMU and no driver can outlive
    /// its task with an armed queue; a driver that owns a device must stop the
    /// queue (or reset it and observe completion) before it exits.
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
                    frame_allocator::free_contiguous_pages_tagged(
                        physical,
                        page_count,
                        generation,
                    );
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
        self.heap_pages.clear();
        self.dma_pages.clear();
        self.mmap_regions.clear();
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
    #[cfg(feature = "fog-paint")]
    unsafe {
        // No stack, no Rust call: paint brown as the very first idle
        // instruction. If SP from the context is bad, this still tells us
        // that the context switch reached the idle entry point.
        core::arch::asm!(
            // MMU is on here: use the high-half alias, not the physical
            // address. `phys_to_virt(0x5C00_0000)` is 0xFFFF_0000_5C00_0000.
            "mov x9, #0x5C000000",
            "movk x9, #0xFFFF, lsl #48",
            "mov x10, #0x5C800000",
            "movk x10, #0xFFFF, lsl #48",
            "mov w11, #0x4000",
            "movk w11, #0x8040, lsl #16",
            "1:",
            "str w11, [x9], #4",
            "cmp x9, x10",
            "b.lo 1b",
            options(nostack),
        );
    }
    // First dispatch on a CPU enters here with the kernel lock held by the
    // dispatching `schedule` (user-mode bootstrap drops it in asm; kernel
    // entries do it here). Idle never holds the lock again: it sleeps in
    // `wfi` so other CPUs can schedule while this CPU waits for an IRQ.
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFF00_FF00); // green: idle task entered
    interrupts::enable_irq();
    #[cfg(feature = "fog-paint")]
    crate::fog::paint(0xFF00_00FF); // blue: idle waiting for an IRQ
    #[cfg(feature = "fog-ui")]
    {
        // Live status panel. IRQs are enabled above, so the 100 Hz tick keeps
        // preempting this loop and the scheduler keeps running underneath.
        crate::fog_ui::run()
    }
    #[cfg(all(not(feature = "fog-ui"), feature = "fog-diag"))]
    {
        // "Immediately before the idle wait": IRQs unmasked, timer armed,
        // scheduler has not dispatched anything yet. The paged display then
        // runs instead of `WFI` so the operator can photograph every page.
        crate::gic_diag::capture(2);
        crate::gic_diag::run_display()
    }
    #[cfg(all(not(feature = "fog-ui"), not(feature = "fog-diag")))]
    loop {
        interrupts::wait_for_irq();
    }
}
