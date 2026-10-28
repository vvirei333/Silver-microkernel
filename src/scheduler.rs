use core::cell::UnsafeCell;

use alloc::{string::String, vec::Vec};

use crate::{
    capability,
    elf::LoadedImage,
    elf_loader,
    frame_allocator, interrupts,
    memory::{phys_to_virt, virt_to_phys, MemoryRegion, PAGE_SIZE},
    mmu, mutex, pl011, timer, user_program, vfs,
    wait_queue::WaitQueue,
};

const TASK_STACK_PAGES: usize = 4;
const TASK_STACK_SIZE: usize = TASK_STACK_PAGES * PAGE_SIZE;
const TASK_USER_STACK_PAGES: usize = mmu::USER_STACK_PAGES;
const TASK_USER_STACK_SIZE: usize = mmu::USER_STACK_SIZE;
const IDLE_TASK_ID: u64 = 0;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Ready,
    Running,
    Sleeping(u64),
    Blocked,
    BlockedUntil(u64),
    Stopped,
    Dead,
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
    cwd: String,
    owns_console: bool,
    pub context: TaskContext,
    pub stack: MemoryRegion,
    pub user_stack: MemoryRegion,
    pub ttbr0: usize,
    user_image: Option<LoadedImage>,
    heap_pages: Vec<usize>,
    dma_pages: Vec<(usize, usize)>,
    program_break: usize,
    waiting_on: Option<*const WaitQueue>,
    owned_mutexes: Vec<*mut mutex::MutexState>,
}

pub struct TaskManager {
    tasks: Vec<Task>,
    current_index: usize,
    next_id: u64,
    console_parent: Option<u64>,
    exit_statuses: Vec<(u64, u64)>,
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
    pl011::write_str("SWITCH TRACE from,to,sp,elr:\r\n");
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
            pl011::write_dec_u64(from);
            pl011::write_str("->");
            pl011::write_dec_u64(to);
            pl011::write_str(" sp=");
            pl011::write_hex_u64(sp);
            pl011::write_str(" elr=");
            pl011::write_hex_u64(elr);
            pl011::write_str("\r\n");
        }
    }
}

struct SharedScheduler(UnsafeCell<Option<TaskManager>>);
unsafe impl Sync for SharedScheduler {}

static SCHEDULER: SharedScheduler = SharedScheduler(UnsafeCell::new(None));
static PROCESS_WAIT_QUEUE: WaitQueue = WaitQueue::new();
static mut BOOT_CONTEXT: TaskContext = TaskContext::zero();

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
    mov x0, x20
    msr elr_el1, x19
    mov x9, xzr
    msr spsr_el1, x9
    eret
    .size silver_user_bootstrap, . - silver_user_bootstrap
"#
);

extern "C" {
    fn silver_context_switch(current: *mut TaskContext, next: *const TaskContext);
    fn silver_user_bootstrap();
}

pub fn init() {
    let slot = scheduler_slot();
    *slot = Some(TaskManager {
        tasks: Vec::new(),
        current_index: 0,
        next_id: 0,
        console_parent: None,
        exit_statuses: Vec::new(),
    });

    let _idle_id = spawn_kernel(idle_task);

    #[cfg(not(feature = "demo-tasks"))]
    {
        let silverd_id =
            spawn_user(user_program::SILVERD, 0).expect("silverd must be created at boot");
        let _ = set_console_owner(silverd_id);
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
    }

    let first_id = 1;
    let manager = scheduler_manager().unwrap();
    manager.current_index = manager
        .tasks
        .iter()
        .position(|task| task.id == first_id)
        .unwrap_or(0);
}

pub fn yield_current() {
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        if task.state == TaskState::Running {
            task.state = TaskState::Ready;
        }
    }
   
    schedule();
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
    schedule();
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
        schedule();
    }
}

pub fn irq_is_bound(intid: u32) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager
                .tasks
                .iter()
                .any(|task| task.state != TaskState::Dead && task.bound_irq == Some(intid))
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
        .find(|task| task.state != TaskState::Dead && task.bound_irq == Some(intid))
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
                task.state != TaskState::Dead && task.bound_irq == Some(crate::gic::UART_IRQ)
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
                    task.id == id && task.state != TaskState::Dead && task.bound_irq.is_some()
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

pub fn register_waiter(queue: &WaitQueue) {
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(queue as *const WaitQueue);
    }
}

pub fn block_current_on(queue: &WaitQueue) {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(queue as *const WaitQueue);
        task.state = TaskState::Blocked;
    }
    schedule();
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn block_current_on_until(queue: &WaitQueue, deadline: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(queue as *const WaitQueue);
        task.state = TaskState::BlockedUntil(deadline);
    }
    schedule();
    queue.remove(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn wake_task(id: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(manager) = scheduler_manager() {
        if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
            wake_blocked_task(task);
        }
    }
    interrupts::restore_irq(saved_daif);
}

pub fn register_mutex(state: *mut mutex::MutexState) {
    if let Some(task) = current_task_mut() {
        if !task.owned_mutexes.contains(&state) {
            task.owned_mutexes.push(state);
        }
    }
}

pub fn unregister_mutex(state: *mut mutex::MutexState) {
    if let Some(task) = current_task_mut() {
        task.owned_mutexes.retain(|owned| *owned != state);
    }
}

const EXIT_STATUS_LIMIT: usize = 64;

pub fn exit_current() -> ! {
    exit_current_with_status(0)
}

pub fn exit_current_with_status(status: u64) -> ! {
    let id = current_task_id();
    // Kernel-side trace: syscall::print is discarded for tasks that do not own
    // the console, so a task dying silently leaves no user-visible evidence.
    // pl011 writes here are raw MMIO pokes and stay visible for every task.
    let program = current_task_mut().map_or(u64::MAX, |task| task.program);
    pl011::write_str("EXIT: task=");
    pl011::write_dec_u64(id);
    pl011::write_str(" program=");
    pl011::write_dec_u64(program);
    pl011::write_str(" status=");
    pl011::write_dec_u64(status);
    // Packed reports (see the filesystem server's mount failure) are unreadable
    // in decimal, so the same value is repeated in hex on the same line.
    pl011::write_str(" (0x");
    pl011::write_hex_u64(status);
    pl011::write_str(")\r\n");
    crate::ipc::remove_task(id);
    vfs::close_task(id);
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        task.state = TaskState::Dead;
        task.bound_irq = None;
    }
    if let Some(manager) = scheduler_manager() {
        recover_console(manager, id);
        record_exit(manager, id, status);
    }
    schedule();
    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

pub fn kill_task(id: u64) -> bool {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return false;
    }
    crate::ipc::remove_task(id);
    vfs::close_task(id);
    let saved_daif = interrupts::save_and_disable_irq();
    let Some(manager) = scheduler_manager() else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    if task.state == TaskState::Dead {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    task.state = TaskState::Dead;
    task.bound_irq = None;
    recover_console(manager, id);
    record_exit(manager, id, 1);
    schedule();
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
        .find(|task| task.state != TaskState::Dead && task.program == user_program::HOWL)
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
    match wait_inner(id, false) {
        Some(WaitEvent::Exited(status)) => Some(status),
        _ => None,
    }
}

pub fn wait_for_task_event(id: u64) -> Option<WaitEvent> {
    wait_inner(id, true)
}

fn wait_inner(id: u64, report_stopped: bool) -> Option<WaitEvent> {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return None;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    loop {
        let Some(manager) = scheduler_manager() else {
            interrupts::restore_irq(saved_daif);
            return None;
        };
        if let Some(index) = manager
            .exit_statuses
            .iter()
            .position(|(task_id, _)| *task_id == id)
        {
            let (_, status) = manager.exit_statuses.remove(index);
            if let Some(waiter) = manager.tasks.get_mut(manager.current_index) {
                waiter.cap_table.remove_for_target(id);
            }
            interrupts::restore_irq(saved_daif);
            return Some(WaitEvent::Exited(status));
        }
        let Some(task) = manager.tasks.iter().find(|task| task.id == id) else {
            interrupts::restore_irq(saved_daif);
            return None;
        };
        if report_stopped && task.state == TaskState::Stopped {
            interrupts::restore_irq(saved_daif);
            return Some(WaitEvent::Stopped);
        }
        let waiter = current_task_id();
        PROCESS_WAIT_QUEUE.enqueue(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = Some(&PROCESS_WAIT_QUEUE);
            task.state = TaskState::Blocked;
        }
        schedule();
        PROCESS_WAIT_QUEUE.remove(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = None;
        }
    }
}

fn record_exit(manager: &mut TaskManager, id: u64, status: u64) {
    if let Some(record) = manager
        .exit_statuses
        .iter_mut()
        .find(|(task_id, _)| *task_id == id)
    {
        *record = (id, status);
    } else {
        if manager.exit_statuses.len() == EXIT_STATUS_LIMIT {
            manager.exit_statuses.remove(0);
        }
        manager.exit_statuses.push((id, status));
    }
    wake_process_waiters(manager);
}

fn wake_process_waiters(manager: &mut TaskManager) {
    while let Some(waiter) = PROCESS_WAIT_QUEUE.pop() {
        if let Some(task) = manager
            .tasks
            .iter_mut()
            .find(|task| task.id == waiter && task.state != TaskState::Dead)
        {
            wake_blocked_task(task);
        }
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
        .and_then(|manager| manager.tasks.get(manager.current_index))
        .map(|task| task.id)
        .unwrap_or(0)
}

pub fn current_task_owns_console() -> bool {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(manager.current_index))
        .map(|task| task.owns_console)
        .unwrap_or(false)
}

pub fn stop_console_owner() -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    let mut stopped = false;
    if let Some(manager) = scheduler_manager() {
        if manager.console_parent.is_some() {
            if let Some(task) = manager
                .tasks
                .iter_mut()
                .find(|task| task.owns_console && task.state != TaskState::Dead)
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
                .find(|task| task.id == id && task.state != TaskState::Dead)
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
                    .any(|task| task.id == id && task.state != TaskState::Dead);
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
                .any(|task| task.id == id && task.state != TaskState::Dead)
        })
        .unwrap_or(false)
}

pub fn current_task_arguments() -> Vec<u8> {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(manager.current_index))
        .map(|task| task.arguments.clone())
        .unwrap_or_default()
}

pub fn current_task_cwd() -> String {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(manager.current_index))
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"))
}

pub fn cwd_in_use(path: &str) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                task.state != TaskState::Dead
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
    }
}

pub fn change_program_break(requested: usize) -> Option<usize> {
    current_task_mut()?.change_program_break(requested)
}

pub fn start() -> ! {
    let saved_daif = interrupts::save_and_disable_irq();
    let manager = scheduler_manager().unwrap();
    let current_index = manager.current_index;
    let Some(first_task) = manager.tasks.get_mut(current_index) else {
        loop {
            interrupts::wait_for_irq();
        }
    };

    first_task.state = TaskState::Running;
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

pub fn schedule() {
    let Some(manager) = scheduler_manager() else {
        return;
    };
    if manager.tasks.is_empty() {
        return;
    }

    let live_task_id = manager
        .tasks
        .get(manager.current_index)
        .map(|task| task.id)
        .unwrap_or(IDLE_TASK_ID);
    let live_is_dead = manager
        .tasks
        .get(manager.current_index)
        .map(|task| task.state == TaskState::Dead)
        .unwrap_or(false);

    reap_dead_tasks(manager);
    wake_expired_tasks(manager, timer::ticks());

    if manager.current_index >= manager.tasks.len() {
        manager.current_index = 0;
    }

    let current_index = manager.current_index;
    if manager.tasks[current_index].state == TaskState::Running {
        manager.tasks[current_index].state = TaskState::Ready;
    }

    let Some(next_index) = find_next_task(manager) else {
        return;
    };
    if next_index == current_index && !live_is_dead {
        return;
    }

    let tasks = manager.tasks.as_mut_ptr();
    let next_task = unsafe { &mut *tasks.add(next_index) };
    let saved_stack_pointer = next_task.context.stack_pointer as usize;
    if !next_task
        .stack
        .contains(virt_to_phys(saved_stack_pointer) - 1)
    {
        pl011::write_str("BAD SP task=");
        pl011::write_dec_u64(next_task.id);
        pl011::write_str(" sp=");
        pl011::write_hex_u64(next_task.context.stack_pointer);
        pl011::write_str(" stack=[");
        pl011::write_hex_u64(next_task.stack.start as u64);
        pl011::write_str(",");
        pl011::write_hex_u64(next_task.stack.end as u64);
        pl011::write_str("]\r\n");
        panic!("bad saved stack pointer");
    }

    let next_id = next_task.id;
    let next_ttbr0 = next_task.ttbr0;
    next_task.state = TaskState::Running;
    manager.current_index = next_index;

    let current_context = if live_is_dead {
        unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) }
    } else {
        unsafe { &mut (*tasks.add(current_index)).context }
    };
    let from_id = if live_is_dead {
        live_task_id
    } else {
        unsafe { (*tasks.add(current_index)).id }
    };
    let current_ttbr0 = unsafe { (*tasks.add(current_index)).ttbr0 };

    if live_is_dead || next_ttbr0 != current_ttbr0 {
        mmu::switch_address_space(next_ttbr0);
    }

    trace_switch(
        from_id,
        next_id,
        next_task.context.stack_pointer,
        next_task.context.elr,
    );

    unsafe {
        silver_context_switch(current_context, &next_task.context);
    }
}

fn spawn_kernel(entry: extern "C" fn() -> !) -> Option<u64> {
    spawn_inner(Some(entry), None, Vec::new())
}

pub fn spawn_user(program: u64, argument: u64) -> Option<u64> {
    spawn_inner(None, Some((program, argument)), Vec::new())
}

pub fn spawn_user_with_cap(program: u64, argument: u64) -> Option<capability::CapId> {
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
    let task_id = spawn_elf_task(image, argument)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

fn spawn_elf_task(image: LoadedImage, argument: u64) -> Option<u64> {
    let manager = scheduler_manager()?;
    let id = manager.next_id;
    let cwd = manager
        .tasks
        .get(manager.current_index)
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    let entry = image.entry;
    let stack_va = mmu::USER_STACK_VA;
    let mut task = Task::new(
        id,
        None,
        Some((user_program::PROGRAM_DYNAMIC, argument)),
        Some(image),
        Vec::new(),
    )?;
    task.cwd = cwd;
    manager.tasks.push(task);
    manager.next_id += 1;
    pl011::write_str("Spawning ELF at 0x");
    pl011::write_hex_u64(entry as u64);
    pl011::write_str(" with stack at 0x");
    pl011::write_hex_u64(stack_va as u64);
    pl011::write_str("\r\n");
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

pub fn grant_capability(cap_id: capability::CapId, recipient_task_id: u64, rights: u32) -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some((target_task, rights)) =
        current_task_mut().and_then(|task| task.cap_table.grant(cap_id, rights))
    {
        if let Some(manager) = scheduler_manager() {
            if let Some(recipient) = manager
                .tasks
                .iter_mut()
                .find(|task| task.id == recipient_task_id && task.state != TaskState::Dead)
            {
                recipient.cap_table.insert(target_task, rights);
                granted = true;
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    granted
}

pub fn current_task_has_rights(rights: u32) -> bool {
    current_task_mut()
        .map(|task| task.cap_table.has_rights(rights))
        .unwrap_or(false)
}

pub fn map_phys_to_current(phys_addr: usize, size: usize) -> Option<usize> {
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let shared_dma = manager.tasks.iter().any(|owner| {
            owner.dma_pages.iter().any(|&(base, page_count)| {
                base.checked_add(page_count * PAGE_SIZE)
                    .is_some_and(|dma_end| {
                        phys_addr >= base
                            && phys_addr
                                .checked_add(size)
                                .is_some_and(|requested_end| requested_end <= dma_end)
                    })
            })
        });
        let task = manager.tasks.get_mut(manager.current_index)?;
        if !task.cap_table.has_rights(capability::CAP_MMIO) {
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
        let task = manager.tasks.get_mut(manager.current_index)?;
        if !task.cap_table.has_rights(capability::CAP_MMIO) {
            pl011::write_str("DMA: FAIL - no CAP_MMIO rights\r\n");
            return None;
        }
        pl011::write_str("DMA: has CAP_MMIO, allocating ");
        pl011::write_hex_u64(page_count as u64);
        pl011::write_str(" pages\r\n");
        let physical = match frame_allocator::alloc_contiguous_pages(page_count) {
            Some(p) => {
                pl011::write_str("DMA: alloc_contiguous_pages OK, physical= ");
                pl011::write_hex_u64(p as u64);
                pl011::write_str("\r\n");
                p
            }
            None => {
                pl011::write_str("DMA: FAIL - alloc_contiguous_pages returned None\r\n");
                return None;
            }
        };
        unsafe { core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, mapped_size) };
        pl011::write_str("DMA: virt addr = ");
        pl011::write_hex_u64(phys_to_virt(physical) as u64);
        pl011::write_str("\r\n");
        pl011::write_str("DMA: zeroing...\r\n");
        pl011::write_str("DMA: zeroing OK\r\n");
        pl011::write_str("DMA: mapping to user...\r\n");
        let Some(virtual_address) = mmu::map_dma_to_user(task, physical, mapped_size) else {
            pl011::write_str("DMA: FAIL - mmu::map_dma_to_user returned None\r\n");
            release_pages(physical, page_count);
            return None;
        };
        pl011::write_str("DMA: mapped va=");
        pl011::write_hex_u64(virtual_address as u64);
        pl011::write_str("\r\n");
        task.dma_pages.push((physical, page_count));
        Some((virtual_address, physical))
    });
    interrupts::restore_irq(saved_daif);
    result
}

pub fn bind_irq(intid: u32) -> bool {
    if intid >= 1020 || intid == crate::gic::TIMER_IRQ {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager()
        .map(|manager| {
            let current_index = manager.current_index;
            let Some(current) = manager.tasks.get(current_index) else {
                return false;
            };
            if !current.cap_table.has_rights(capability::CAP_IRQ) {
                return false;
            }
            if manager.tasks.iter().enumerate().any(|(index, task)| {
                index != current_index
                    && task.state != TaskState::Dead
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
    let task_id = spawn_user_with_args(program, argument, arguments)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

pub fn spawn_user_with_args(program: u64, argument: u64, arguments: Vec<u8>) -> Option<u64> {
    spawn_inner(None, Some((program, argument)), arguments)
}

fn spawn_inner(
    entry: Option<extern "C" fn() -> !>,
    user: Option<(u64, u64)>,
    arguments: Vec<u8>,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    let id = manager.next_id;
    pl011::write_str("SPAWN: start id=");
    pl011::write_dec_u64(id);
    pl011::write_str(" program=");
    pl011::write_dec_u64(user.map_or(u64::MAX, |(program, _)| program));
    pl011::write_str("\r\n");

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

    let cwd = manager
        .tasks
        .get(manager.current_index)
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    let mut task = Task::new(id, entry, user, image, arguments)?;
    task.cwd = cwd;

    manager.tasks.push(task);
    manager.next_id += 1;
    pl011::write_str("SPAWN: success id=");
    pl011::write_dec_u64(id);
    pl011::write_str("\r\n");
    Some(id)
}

fn find_next_task(manager: &TaskManager) -> Option<usize> {
    let task_count = manager.tasks.len();

    for offset in 1..=task_count {
        let index = (manager.current_index + offset) % task_count;
        let task = &manager.tasks[index];
        if task.id != IDLE_TASK_ID && task.state == TaskState::Ready {
            return Some(index);
        }
    }

    for offset in 1..=task_count {
        let index = (manager.current_index + offset) % task_count;
        if manager.tasks[index].state == TaskState::Ready {
            return Some(index);
        }
    }

    None
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
                task.resume_state = Some(TaskState::Ready);
            }
            continue;
        }
        let wake_at = match task.state {
            TaskState::Sleeping(deadline) | TaskState::BlockedUntil(deadline) => deadline,
            _ => continue,
        };
        if now >= wake_at {
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

    let dead_ids: Vec<u64> = manager
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Dead)
        .map(|task| task.id)
        .collect();

    let live_id = manager.tasks.get(manager.current_index).map(|task| task.id);

    let mut wake = Vec::new();
    for task in manager
        .tasks
        .iter_mut()
        .filter(|task| task.state == TaskState::Dead)
    {
        if let Some(queue) = task.waiting_on.take() {
            unsafe { &*queue }.remove(task.id);
        }
        wake.extend(task.release_mutexes());
        task.cap_table.remove_all();
        task.bound_irq = None;
        task.irq_pending = false;
    }
    for id in wake {
        if let Some(task) = manager
            .tasks
            .iter_mut()
            .find(|task| task.id == id && task.state != TaskState::Dead)
        {
            wake_blocked_task(task);
        }
    }

    for task in manager
        .tasks
        .iter_mut()
        .filter(|task| task.state != TaskState::Dead)
    {
        for dead_id in &dead_ids {
            task.cap_table.retire_for_target(*dead_id);
        }
    }

    manager.tasks.retain(|task| {
        if task.state == TaskState::Dead {
            task.release_stack();
            task.release_space();
            false
        } else {
            true
        }
    });

    manager.current_index = live_id
        .and_then(|id| manager.tasks.iter().position(|task| task.id == id))
        .unwrap_or(0);
}

fn current_task_mut() -> Option<&'static mut Task> {
    let manager = scheduler_manager()?;
    let index = manager.current_index;
    let tasks = manager.tasks.as_mut_ptr();
    unsafe { manager.tasks.get_mut(index).map(|_| &mut *tasks.add(index)) }
}

impl Task {
    fn new(
        id: u64,
        entry: Option<extern "C" fn() -> !>,
        user: Option<(u64, u64)>,
        image: Option<LoadedImage>,
        arguments: Vec<u8>,
    ) -> Option<Self> {
        let program_label = user.map_or(u64::MAX, |(program, _)| program);
        pl011::write_str("SPAWN: alloc stack (");
        pl011::write_dec_u64(TASK_STACK_PAGES as u64);
        pl011::write_str(" pages)...\r\n");
        let Some(stack_start) = frame_allocator::alloc_contiguous_pages(TASK_STACK_PAGES) else {
            pl011::write_str("SPAWN: FAIL - no contiguous kernel stack run\r\n");
            return None;
        };
        pl011::write_str("SPAWN: kernel stack at ");
        pl011::write_hex_u64(stack_start as u64);
        pl011::write_str("\r\n");

        let user_stack = if user.is_some() {
            pl011::write_str("SPAWN: alloc user stack...\r\n");
            let Some(user_start) = frame_allocator::alloc_contiguous_pages(TASK_USER_STACK_PAGES)
            else {
                pl011::write_str("SPAWN: FAIL - no contiguous user stack run\r\n");
                release_pages(stack_start, TASK_STACK_PAGES);
                return None;
            };
            pl011::write_str("SPAWN: user stack at ");
            pl011::write_hex_u64(user_start as u64);
            pl011::write_str("\r\n");
            MemoryRegion::new(user_start, user_start + TASK_USER_STACK_SIZE)
        } else {
            MemoryRegion::new(0, 0)
        };

        pl011::write_str("SPAWN: alloc private page...\r\n");
        let Some(private_page) = frame_allocator::alloc_page() else {
            pl011::write_str("SPAWN: FAIL - no frame for the private window\r\n");
            if !user_stack.is_empty() {
                release_pages(user_stack.start, TASK_USER_STACK_PAGES);
            }
            release_pages(stack_start, TASK_STACK_PAGES);
            return None;
        };

        let user_image = image;

        pl011::write_str("SPAWN: build task space (9 tables)...\r\n");
        let Some(ttbr0) =
            mmu::create_task_space(private_page, user_stack.start, user_image.as_ref())
        else {
            pl011::write_str("SPAWN: FAIL - mmu::create_task_space returned None\r\n");
            if let Some(image) = &user_image {
                image.release();
            }
            frame_allocator::free_page(private_page);
            if !user_stack.is_empty() {
                release_pages(user_stack.start, TASK_USER_STACK_PAGES);
            }
            release_pages(stack_start, TASK_STACK_PAGES);
            return None;
        };
        pl011::write_str("SPAWN: task space ttbr0=");
        pl011::write_hex_u64(ttbr0 as u64);
        pl011::write_str("\r\n");

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
        context.user_stack_pointer = if user.is_some() {
            (mmu::USER_STACK_VA + mmu::USER_STACK_SIZE) as u64
        } else {
            0
        };

        pl011::write_str("SPAWN: registers set program=");
        pl011::write_dec_u64(program_label);
        pl011::write_str("\r\n");

        let mut cap_table = capability::CapTable::new();
        match user.map(|(program, _)| program) {
            Some(user_program::SILVERD) => {
                cap_table.insert(id, capability::CAP_ALL); // <--- ДОБАВЛЕНО
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
            _ => {}
        }

        Some(Self {
            id,
            state: TaskState::Ready,
            resume_state: None,
            program: user.map_or(u64::MAX, |(program, _)| program),
            argument: user.map_or(0, |(_, argument)| argument),
            cap_table,
            bound_irq: None,
            irq_pending: false,
            arguments,
            cwd: String::from("/"),
            owns_console: cfg!(feature = "demo-tasks"),
            context,
            stack,
            user_stack,
            ttbr0,
            user_image,
            heap_pages: Vec::new(),
            dma_pages: Vec::new(),
            program_break: mmu::USER_HEAP_VA,
            waiting_on: None,
            owned_mutexes: Vec::new(),
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
            let Some(frame) = frame_allocator::alloc_page() else {
                self.truncate_heap(original_pages);
                return None;
            };
            if !mmu::map_user_heap_page(self.ttbr0, index, frame) {
                frame_allocator::free_page(frame);
                self.truncate_heap(original_pages);
                return None;
            }
            self.heap_pages.push(frame);
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
            debug_assert_eq!(mapped, tracked);
            frame_allocator::free_page(mapped);
        }
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
        release_pages(self.stack.start, TASK_STACK_PAGES);
        if !self.user_stack.is_empty() {
            release_pages(self.user_stack.start, TASK_USER_STACK_PAGES);
        }
    }

    fn release_space(&self) {
        mmu::destroy_task_space(self.ttbr0);
        for &(physical, page_count) in &self.dma_pages {
            release_pages(physical, page_count);
        }
        if let Some(image) = &self.user_image {
            image.release();
        }
    }
}

fn release_pages(start: usize, page_count: usize) {
    for index in 0..page_count {
        frame_allocator::free_page(start + index * PAGE_SIZE);
    }
}

fn scheduler_slot() -> &'static mut Option<TaskManager> {
    unsafe { &mut *SCHEDULER.0.get() }
}

fn scheduler_manager() -> Option<&'static mut TaskManager> {
    scheduler_slot().as_mut()
}

extern "C" fn idle_task() -> ! {
    interrupts::enable_irq();
    loop {
        interrupts::wait_for_irq();
    }
}
