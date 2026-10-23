use core::cell::UnsafeCell;

use alloc::vec::Vec;

use crate::{
    frame_allocator, interrupts, ipc,
    memory::{MemoryRegion, PAGE_SIZE},
    mutex, nameserver, pl011, syscall, timer,
};

const TASK_STACK_PAGES: usize = 4;
const TASK_STACK_SIZE: usize = TASK_STACK_PAGES * PAGE_SIZE;
const X30_INDEX: usize = 11;
const IDLE_TASK_ID: u64 = 0;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Ready,
    Running,
    Sleeping(u64),
    Blocked,
    Dead,
}

#[repr(C, align(16))]
pub struct TaskContext {
    registers: [u64; 12],
    stack_pointer: u64,
}

impl TaskContext {
    const fn zero() -> Self {
        Self {
            registers: [0; 12],
            stack_pointer: 0,
        }
    }
}

pub struct Task {
    pub id: u64,
    pub state: TaskState,
    pub context: TaskContext,
    pub stack: MemoryRegion,
}

pub struct TaskManager {
    tasks: Vec<Task>,
    current_index: usize,
    next_id: u64,
}

struct SharedScheduler(UnsafeCell<Option<TaskManager>>);

// Public scheduler entry points disable IRQs while mutating TaskManager. Timer IRQs
// enter schedule() already masked. Add a real lock when SMP support appears.
unsafe impl Sync for SharedScheduler {}

static SCHEDULER: SharedScheduler = SharedScheduler(UnsafeCell::new(None));
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

    ldr x9, [x1, #96]
    mov sp, x9
    ldp x19, x20, [x1, #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    ret
    .size silver_context_switch, . - silver_context_switch
"#
);

extern "C" {
    fn silver_context_switch(current: *mut TaskContext, next: *const TaskContext);
}

pub fn init() {
    let slot = scheduler_slot();
    *slot = Some(TaskManager {
        tasks: Vec::new(),
        current_index: 0,
        next_id: 0,
    });

    // Idle always exists, so the CPU has somewhere to run when all tasks sleep.
    let _idle_id = spawn_inner(idle_task);
    // Spawned first after idle, so its ID is NAMESERVER_ID (1) — never
    // reorder without updating syscall::NAMESERVER_ID.
    let _nameserver_id = spawn_inner(nameserver::nameserver_task);
    let _holder_id = spawn_inner(mutex_holder);
    let _contender_1_id = spawn_inner(mutex_contender);
    let _contender_2_id = spawn_inner(mutex_contender);
    let _ping_id = spawn_inner(ipc_ping);
    let _pong_id = spawn_inner(ipc_pong);
    // Condvar demo: alpha and beta both block in receive(); the probe sends
    // to alpha only — beta must stay asleep.
    let _alpha_id = spawn_inner(condvar_alpha);
    let _beta_id = spawn_inner(condvar_beta);
    let _probe_id = spawn_inner(condvar_probe);
    let _sleeper_id = spawn_inner(sleeping_task);

    let manager = scheduler_manager().unwrap();
    manager.current_index = manager
        .tasks
        .iter()
        .position(|task| task.id == 1)
        .unwrap_or(0);
}

/// Dynamically adds a task and returns its ID.
pub fn spawn(entry: extern "C" fn() -> !) -> Option<u64> {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = spawn_inner(entry);
    interrupts::restore_irq(saved_daif);
    id
}

/// Voluntarily gives up the remainder of the current time slice.
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

/// Puts the current task to sleep for approximately the requested duration.
pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        yield_current();
        return;
    }

    let saved_daif = interrupts::save_and_disable_irq();
    let wake_at = timer::ticks() + timer::milliseconds_to_ticks(milliseconds);

    if let Some(task) = current_task_mut() {
        task.state = TaskState::Sleeping(wake_at);
    }

    schedule();
    interrupts::restore_irq(saved_daif);
}

/// Marks the current task as Blocked and switches away. The task only runs
/// again after wake_task() moves it back to the ready queue.
pub fn block_current() {
    let saved_daif = interrupts::save_and_disable_irq();

    if let Some(task) = current_task_mut() {
        task.state = TaskState::Blocked;
    }

    schedule();
    interrupts::restore_irq(saved_daif);
}

/// Moves a blocked task back to the ready queue. No-op for tasks that are
/// not blocked (e.g. a sleeper that was never enqueued).
pub fn wake_task(id: u64) {
    let saved_daif = interrupts::save_and_disable_irq();

    if let Some(manager) = scheduler_manager() {
        if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
            }
        }
    }

    interrupts::restore_irq(saved_daif);
}

/// Terminates the current task and switches to another runnable task.
pub fn exit_current() -> ! {
    let saved_daif = interrupts::save_and_disable_irq();

    if let Some(task) = current_task_mut() {
        task.state = TaskState::Dead;
    }

    schedule();
    interrupts::restore_irq(saved_daif);

    loop {
        interrupts::wait_for_irq();
    }
}

pub fn current_task_id() -> u64 {
    scheduler_manager()
        .and_then(|manager| manager.tasks.get(manager.current_index))
        .map(|task| task.id)
        .unwrap_or(0)
}

/// Switches from the boot context into the first task. This function never returns.
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
    let first_context = &first_task.context as *const TaskContext;
    let boot_context = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) };

    unsafe {
        silver_context_switch(boot_context, first_context);
    }

    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

/// Wakes expired sleepers, skips sleeping tasks, and switches to the next ready task.
pub fn schedule() {
    let Some(manager) = scheduler_manager() else {
        return;
    };
    if manager.tasks.is_empty() {
        return;
    }

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
    if next_index == current_index {
        return;
    }

    let tasks = manager.tasks.as_mut_ptr();
    // The indices are different, so these are disjoint tasks.
    let current_task = unsafe { &mut *tasks.add(current_index) };
    let next_task = unsafe { &mut *tasks.add(next_index) };

    debug_assert_ne!(current_task.id, next_task.id);
    debug_assert!(next_task
        .stack
        .contains(next_task.context.stack_pointer as usize - 1));

    next_task.state = TaskState::Running;
    manager.current_index = next_index;

    unsafe {
        silver_context_switch(
            &mut current_task.context as *mut TaskContext,
            &next_task.context as *const TaskContext,
        );
    }
}

fn spawn_inner(entry: extern "C" fn() -> !) -> Option<u64> {
    let manager = scheduler_manager()?;
    let id = manager.next_id;
    let task = Task::new(id, entry)?;

    manager.tasks.push(task);
    manager.next_id += 1;
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
        if let TaskState::Sleeping(wake_at) = task.state {
            if now >= wake_at {
                task.state = TaskState::Ready;
            }
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

    manager.tasks.retain(|task| {
        if task.state == TaskState::Dead {
            task.release_stack();
            false
        } else {
            true
        }
    });

    manager.current_index = manager
        .tasks
        .iter()
        .position(|task| task.state == TaskState::Running)
        .unwrap_or(0);
}

fn current_task_mut() -> Option<&'static mut Task> {
    let manager = scheduler_manager()?;
    let index = manager.current_index;
    let tasks = manager.tasks.as_mut_ptr();
    unsafe { manager.tasks.get_mut(index).map(|_| &mut *tasks.add(index)) }
}

impl Task {
    fn new(id: u64, entry: extern "C" fn() -> !) -> Option<Self> {
        let stack_start = frame_allocator::alloc_contiguous_pages(TASK_STACK_PAGES)?;
        let stack = MemoryRegion::new(stack_start, stack_start + TASK_STACK_SIZE);
        let mut context = TaskContext::zero();
        context.registers[X30_INDEX] = entry as usize as u64;
        context.stack_pointer = stack.end as u64;

        Some(Self {
            id,
            state: TaskState::Ready,
            context,
            stack,
        })
    }

    fn release_stack(&self) {
        let mut page = self.stack.start;
        while page < self.stack.end {
            frame_allocator::free_page(page);
            page += PAGE_SIZE;
        }
    }
}

fn scheduler_slot() -> &'static mut Option<TaskManager> {
    unsafe { &mut *SCHEDULER.0.get() }
}

fn scheduler_manager() -> Option<&'static mut TaskManager> {
    scheduler_slot().as_mut()
}

extern "C" fn idle_task() -> ! {
    // Voluntary switches leave IRQ masked, so every task entry point must
    // unmask interrupts itself before it can be woken by the timer again.
    interrupts::enable_irq();
    loop {
        interrupts::wait_for_irq();
    }
}

/// Shared counter used by the mutex demo tasks.
static COUNTER: mutex::Mutex<u64> = mutex::Mutex::new(0);

extern "C" fn mutex_holder() -> ! {
    loop {
        interrupts::enable_irq();
        {
            let _guard = COUNTER.lock();
            pl011::write_str("MUTEX HOLDER=1 ACQUIRE\r\n");
            // Hold long enough for both contenders to pile up in the queue.
            sleep(400);
            pl011::write_str("MUTEX HOLDER=1 RELEASE\r\n");
        }
        sleep(50);
    }
}

extern "C" fn mutex_contender() -> ! {
    loop {
        interrupts::enable_irq();
        {
            let mut counter = COUNTER.lock();
            *counter += 1;
            // One byte per successful lock keeps the UART log readable
            // next to the IPC demo lines.
            pl011::write_byte(b'0' + current_task_id() as u8);
        }
        sleep(100);
    }
}

/// Registers `name`, then resolves `peer` through the nameserver, retrying
/// until the peer shows up. Demonstrates the discovery dance both sides do.
fn resolve_peer(name: &str, peer: &str) -> u64 {
    pl011::write_str("IPC REGISTER ");
    pl011::write_str(name);
    pl011::write_str("\r\n");
    if !syscall::register(name) {
        pl011::write_str("IPC REGISTER FAILED\r\n");
    }

    loop {
        if let Some(id) = syscall::lookup(peer) {
            pl011::write_str("IPC FOUND ");
            pl011::write_str(peer);
            pl011::write_str("\r\n");
            return id;
        }
        pl011::write_str("IPC LOOKUP ");
        pl011::write_str(peer);
        pl011::write_str(" RETRY\r\n");
        syscall::sleep(100);
    }
}

extern "C" fn ipc_ping() -> ! {
    interrupts::enable_irq();
    let peer = resolve_peer("ping", "pong");
    let mut reply = ipc::Message::new(0, b"");

    loop {
        pl011::write_str("IPC ping SEND PING (SVC)\r\n");
        syscall::send(peer, &ipc::Message::new(current_task_id(), b"PING"));

        // Blocks here until pong answers.
        syscall::receive(&mut reply);
        pl011::write_str("IPC ping RECV ");
        pl011::write_str(core::str::from_utf8(reply.text()).unwrap_or("?"));
        pl011::write_str(" (SVC)\r\n");

        syscall::sleep(300);
    }
}

extern "C" fn ipc_pong() -> ! {
    interrupts::enable_irq();
    // reply goes to message.sender, so pong only needs to register itself
    resolve_peer("pong", "ping");
    let mut message = ipc::Message::new(0, b"");

    loop {
        // Do other "work" first; the ping sits in our mailbox meanwhile.
        syscall::sleep(200);

        // Blocks here until ping arrives.
        syscall::receive(&mut message);
        pl011::write_str("IPC pong RECV ");
        pl011::write_str(core::str::from_utf8(message.text()).unwrap_or("?"));
        pl011::write_str(" (SVC)\r\n");

        pl011::write_str("IPC pong SEND PONG (SVC)\r\n");
        syscall::send(
            message.sender,
            &ipc::Message::new(current_task_id(), b"PONG"),
        );
    }
}

extern "C" fn sleeping_task() -> ! {
    loop {
        interrupts::enable_irq();
        sleep(500);
        pl011::write_str("TASK ID=");
        pl011::write_hex_u64(current_task_id());
        pl011::write_str(" AWAKE\r\n");
    }
}

/// Condvar demo: two receivers, one message. alpha and beta both block in
/// `receive()`; the probe sends WAKE to alpha only. If the wakeup were not
/// targeted, beta would print its line and the test would fail.

extern "C" fn condvar_alpha() -> ! {
    interrupts::enable_irq();
    syscall::register("alpha");
    let mut message = ipc::Message::new(0, b"");

    loop {
        syscall::receive(&mut message);
        pl011::write_str("CONDVAR alpha RECV ");
        pl011::write_str(core::str::from_utf8(message.text()).unwrap_or("?"));
        pl011::write_str("\r\n");
        // Back to sleep on the mailbox condvar — silent from here on.
    }
}

extern "C" fn condvar_beta() -> ! {
    interrupts::enable_irq();
    syscall::register("beta");
    let mut message = ipc::Message::new(0, b"");

    loop {
        syscall::receive(&mut message);
        // Reaching this print means a foreign wakeup hit beta: TEST FAIL.
        pl011::write_str("CONDVAR beta RECV (TEST FAIL)\r\n");
    }
}

extern "C" fn condvar_probe() -> ! {
    interrupts::enable_irq();
    syscall::register("probe");

    // Wait until both receivers exist, then give them time to reach receive().
    let alpha = loop {
        if let Some(id) = syscall::lookup("alpha") {
            break id;
        }
        syscall::sleep(50);
    };
    loop {
        if syscall::lookup("beta").is_some() {
            break;
        }
        syscall::sleep(50);
    }
    syscall::sleep(300);

    pl011::write_str("CONDVAR TEST: send WAKE to alpha only\r\n");
    syscall::send(alpha, &ipc::Message::new(current_task_id(), b"WAKE"));

    // If beta ever prints, it happens within this window.
    syscall::sleep(500);
    pl011::write_str("CONDVAR TEST: beta silent = PASS\r\n");

    loop {
        syscall::sleep(1000);
    }
}
