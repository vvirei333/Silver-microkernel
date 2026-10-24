use core::cell::UnsafeCell;

use alloc::vec::Vec;

use crate::{
    frame_allocator, interrupts, ipc,
    memory::{phys_to_virt, virt_to_phys, MemoryRegion, PAGE_SIZE},
    mmu, nameserver, pl011, syscall, timer,
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
    Dead,
}

/// Saved execution state of a task. The context switch (`silver_context_switch`)
/// stores/restores this whole block and finishes with `ret`, so every field
/// maps 1:1 onto CPU state:
///   registers          x19-x30 (callee-saved; the trampolines own x0-x18)
///   stack_pointer      SP_EL1 while the task is inside the kernel
///   user_stack_pointer SP_EL0 (user tasks only; 0 for kernel tasks)
///   elr                resume PC, loaded into x30: the task entry point for
///                      a fresh task, or silver_user_bootstrap for a fresh
///                      EL0 task (entry passed in x19); for a running task it
///                      is the saved x30, which returns into schedule() and
///                      unwinds back to the trampoline epilogue (see main.rs)
///
/// Note there is deliberately NO spsr field: resume always lands at EL1h
/// (the `ret` target is EL1 code), and the true return level (EL0t vs EL1h)
/// lives in the exception frame's SPSR slot, restored by the trampoline
/// epilogue.
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

pub struct Task {
    pub id: u64,
    pub state: TaskState,
    pub context: TaskContext,
    /// Kernel stack: used for SVC/IRQ trampolines whenever the task runs.
    pub stack: MemoryRegion,
    /// User stack (SP_EL0) for EL0 tasks; empty for kernel tasks.
    pub user_stack: MemoryRegion,
    /// Physical base of this task's translation tables (TTBR0_EL1 value).
    pub ttbr0: usize,
}

pub struct TaskManager {
    tasks: Vec<Task>,
    current_index: usize,
    next_id: u64,
}

// DEBUG: ring buffer of the last context switches (from, to, next sp, next elr).
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
// Saves the current task's x19-x30 and SP_EL1/SP_EL0 into the block at x0,
// loads the same from the block at x1, and resumes the new task with `ret`.
// x30 holds the resume PC: for a running task it is the return address into
// schedule() (the call chain trampoline -> handler -> schedule unwinds back
// to the trampoline epilogue, which still owns the correct SP and the saved
// ELR/SPSR frame slots); for a fresh task it is the entry point (EL1) or
// silver_user_bootstrap (EL0). ELR_EL1/SPSR_EL1 are deliberately NOT part of
// the switch: they live in the trampoline frames.
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
    str x30, [x0, #112]        // resume PC: return into schedule()

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
    ldr x30, [x1, #112]        // resume PC (elr field) wins over saved x30
    ret
    .size silver_context_switch, . - silver_context_switch

// First entry for a fresh EL0 task. The context switch rets here at EL1h
// with x19 = the task entry point (restored from the initial context) and
// SP_EL0 already loaded. All this stub does is aim eret at the entry with
// SPSR = EL0t and interrupts unmasked.
    .global silver_user_bootstrap
    .type silver_user_bootstrap, %function
silver_user_bootstrap:
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
    });

    // Idle always exists, so the CPU has somewhere to run when all tasks sleep.
    // It stays at EL1: pure wfi loop, never blocks voluntarily, never syscalls.
    let _idle_id = spawn_inner(idle_task, false);
    // Spawned first after idle, so its ID is NAMESERVER_ID (1) — never
    // reorder without updating syscall::NAMESERVER_ID.
    let _nameserver_id = spawn_inner(nameserver::nameserver_task, true);
    let _holder_id = spawn_inner(mutex_holder, true);
    let _contender_1_id = spawn_inner(mutex_contender, true);
    let _contender_2_id = spawn_inner(mutex_contender, true);
    let _ping_id = spawn_inner(ipc_ping, true);
    let _pong_id = spawn_inner(ipc_pong, true);
    // Condvar demo: alpha and beta both block in receive(); the probe sends
    // to alpha only — beta must stay asleep.
    let _alpha_id = spawn_inner(condvar_alpha, true);
    let _beta_id = spawn_inner(condvar_beta, true);
    let _probe_id = spawn_inner(condvar_probe, true);
    let _producer_id = spawn_inner(channel_producer, true);
    let _consumer_id = spawn_inner(channel_consumer, true);
    let _sleeper_id = spawn_inner(sleeping_task, true);
    // Per-task address space demo: same VA, different physical pages.
    let _vm_a_id = spawn_inner(vm_task_a, true);
    let _vm_b_id = spawn_inner(vm_task_b, true);
    // Must die alone on its high-half read; every other task keeps running.
    let _isolation_probe_id = spawn_inner(isolation_probe, true);

    let manager = scheduler_manager().unwrap();
    manager.current_index = manager
        .tasks
        .iter()
        .position(|task| task.id == 1)
        .unwrap_or(0);
}

/// Dynamically adds a task and returns its ID. `user` selects the exception
/// level: user tasks run at EL0t with their own SP_EL0 stack and reach the
/// kernel only through SVC; kernel tasks run at EL1h on the kernel stack.
pub fn spawn(entry: extern "C" fn() -> !, user: bool) -> Option<u64> {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = spawn_inner(entry, user);
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

/// Wakes expired sleepers, skips sleeping tasks, and switches to the next ready task.
pub fn schedule() {
    let Some(manager) = scheduler_manager() else {
        return;
    };
    if manager.tasks.is_empty() {
        return;
    }

    // Capture BEFORE reap: does the task that owns the live CPU context
    // intend to die? reap may remove it from the Vec and reset
    // current_index to 0, after which tasks[current_index] is some innocent
    // task — checking its state would answer the wrong question.
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
    if next_index == current_index {
        return;
    }

    let tasks = manager.tasks.as_mut_ptr();
    // The indices are different, so these are disjoint tasks.
    let current_task = unsafe { &mut *tasks.add(current_index) };
    let next_task = unsafe { &mut *tasks.add(next_index) };

    debug_assert_ne!(current_task.id, next_task.id);
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

    next_task.state = TaskState::Running;
    manager.current_index = next_index;

    // An exiting task's live context is garbage about to be abandoned.
    // Park the mandatory "save" half of the switch in scratch memory so
    // we never overwrite an innocent task's saved state with it.
    let current_context = if live_is_dead {
        unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) }
    } else {
        &mut current_task.context
    };

    if next_task.ttbr0 != current_task.ttbr0 {
        // TTBR1 keeps this high-half code and its kernel stack valid across
        // the switch. Only the user half changes; the TLB flush prevents the
        // next task from observing the previous task's low mappings.
        mmu::switch_address_space(next_task.ttbr0);
    }

    trace_switch(
        current_task.id,
        next_task.id,
        next_task.context.stack_pointer,
        next_task.context.elr,
    );

    unsafe {
        silver_context_switch(
            current_context as *mut TaskContext,
            &next_task.context as *const TaskContext,
        );
    }
}

fn spawn_inner(entry: extern "C" fn() -> !, user: bool) -> Option<u64> {
    let manager = scheduler_manager()?;
    let id = manager.next_id;
    let task = Task::new(id, entry, user)?;

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

    // Remember WHO owns the live CPU context before compacting the Vec.
    // After retain() the old index may be stale or point at a different
    // task; without this, schedule() would save the live context into an
    // innocent task and corrupt its stack pointer.
    let live_id = manager.tasks.get(manager.current_index).map(|task| task.id);

    manager.tasks.retain(|task| {
        if task.state == TaskState::Dead {
            task.release_stack();
            task.release_space();
            false
        } else {
            true
        }
    });

    // If the live task itself was Dead (exit path), it is gone from the
    // list; index 0 is fine because the switch below parks the doomed
    // context in scratch instead of overwriting task 0.
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
    fn new(id: u64, entry: extern "C" fn() -> !, user: bool) -> Option<Self> {
        let stack_start = frame_allocator::alloc_contiguous_pages(TASK_STACK_PAGES)?;

        // Allocate all user-owned memory before building its TTBR0: the
        // tables map the stack at USER_STACK_VA regardless of its PA.
        let user_stack = if user {
            let Some(user_start) = frame_allocator::alloc_contiguous_pages(TASK_USER_STACK_PAGES)
            else {
                release_pages(stack_start, TASK_STACK_PAGES);
                return None;
            };
            MemoryRegion::new(user_start, user_start + TASK_USER_STACK_SIZE)
        } else {
            MemoryRegion::new(0, 0)
        };

        let Some(private_page) = frame_allocator::alloc_page() else {
            if !user_stack.is_empty() {
                release_pages(user_stack.start, TASK_USER_STACK_PAGES);
            }
            release_pages(stack_start, TASK_STACK_PAGES);
            return None;
        };
        let Some(ttbr0) = mmu::create_task_space(private_page, user_stack.start) else {
            frame_allocator::free_page(private_page);
            if !user_stack.is_empty() {
                release_pages(user_stack.start, TASK_USER_STACK_PAGES);
            }
            release_pages(stack_start, TASK_STACK_PAGES);
            return None;
        };

        let stack = MemoryRegion::new(stack_start, stack_start + TASK_STACK_SIZE);
        let mut context = TaskContext::zero();
        context.registers[0] = if user {
            // EL0 executes the low read-only alias of the built-in image.
            virt_to_phys(entry as usize) as u64
        } else {
            entry as usize as u64
        };
        context.elr = if user {
            // The switch `ret`s here at EL1h; the bootstrap drops to EL0t
            // at the low entry alias, passed in x19.
            silver_user_bootstrap as *const () as usize as u64
        } else {
            entry as usize as u64
        };
        context.stack_pointer = phys_to_virt(stack.end) as u64;
        context.user_stack_pointer = if user {
            (mmu::USER_STACK_VA + mmu::USER_STACK_SIZE) as u64
        } else {
            0
        };

        Some(Self {
            id,
            state: TaskState::Ready,
            context,
            stack,
            user_stack,
            ttbr0,
        })
    }

    fn release_stack(&self) {
        release_pages(self.stack.start, TASK_STACK_PAGES);
        if !self.user_stack.is_empty() {
            release_pages(self.user_stack.start, TASK_USER_STACK_PAGES);
        }
    }

    fn release_space(&self) {
        mmu::destroy_task_space(self.ttbr0);
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
    // The only kernel (EL1) task. Its initial context resumes with DAIF
    // masked, so it must unmask IRQ itself; user tasks get unmasked PSTATE
    // from the bootstrap instead.
    interrupts::enable_irq();
    loop {
        interrupts::wait_for_irq();
    }
}

extern "C" fn mutex_holder() -> ! {
    loop {
        syscall::mutex_lock();
        syscall::print("MUTEX HOLDER=1 ACQUIRE\r\n");
        // Sleep WHILE holding the lock: contenders must pile up in the queue.
        syscall::sleep(400);
        syscall::print("MUTEX HOLDER=1 RELEASE\r\n");
        syscall::mutex_unlock();
        syscall::sleep(50);
    }
}

extern "C" fn mutex_contender() -> ! {
    loop {
        syscall::mutex_lock();
        syscall::print("MUTEX T");
        syscall::print_dec_u64(syscall::task_id());
        syscall::print(" LOCK\r\n");
        syscall::mutex_unlock();
        syscall::sleep(100);
    }
}

/// Registers `name`, then resolves `peer` through the nameserver, retrying
/// until the peer shows up. Demonstrates the discovery dance both sides do.
fn resolve_peer(name: &str, peer: &str) -> u64 {
    syscall::print("IPC REGISTER ");
    syscall::print(name);
    syscall::print("\r\n");
    if !syscall::register(name) {
        syscall::print("IPC REGISTER FAILED\r\n");
    }

    loop {
        if let Some(id) = syscall::lookup(peer) {
            syscall::print("IPC FOUND ");
            syscall::print(peer);
            syscall::print("\r\n");
            return id;
        }
        syscall::print("IPC LOOKUP ");
        syscall::print(peer);
        syscall::print(" RETRY\r\n");
        syscall::sleep(100);
    }
}

extern "C" fn ipc_ping() -> ! {
    let peer = resolve_peer("ping", "pong");
    let mut reply = ipc::Message::new(0, b"");

    loop {
        syscall::print("IPC ping SEND PING (SVC)\r\n");
        syscall::send(peer, &ipc::Message::new(syscall::task_id(), b"PING"));

        // Blocks here until pong answers.
        syscall::receive(&mut reply);
        syscall::print("IPC ping RECV ");
        syscall::print(core::str::from_utf8(reply.text()).unwrap_or("?"));
        syscall::print(" (SVC)\r\n");

        syscall::sleep(300);
    }
}

extern "C" fn ipc_pong() -> ! {
    // reply goes to message.sender, so pong only needs to register itself
    resolve_peer("pong", "ping");
    let mut message = ipc::Message::new(0, b"");

    loop {
        // Do other "work" first; the ping sits in our mailbox meanwhile.
        syscall::sleep(200);

        // Blocks here until ping arrives.
        syscall::receive(&mut message);
        syscall::print("IPC pong RECV ");
        syscall::print(core::str::from_utf8(message.text()).unwrap_or("?"));
        syscall::print(" (SVC)\r\n");

        syscall::print("IPC pong SEND PONG (SVC)\r\n");
        syscall::send(
            message.sender,
            &ipc::Message::new(syscall::task_id(), b"PONG"),
        );
    }
}

extern "C" fn sleeping_task() -> ! {
    loop {
        syscall::sleep(500);
        syscall::print("TASK ID=");
        syscall::print_hex_u64(syscall::task_id());
        syscall::print(" AWAKE\r\n");
    }
}

/// Condvar demo: two receivers, one message. alpha and beta both block in
/// `receive()`; the probe sends WAKE to alpha only. If the wakeup were not
/// targeted, beta would print its line and the test would fail.

extern "C" fn condvar_alpha() -> ! {
    syscall::register("alpha");
    let mut message = ipc::Message::new(0, b"");

    loop {
        syscall::receive(&mut message);
        syscall::print("CONDVAR alpha RECV ");
        syscall::print(core::str::from_utf8(message.text()).unwrap_or("?"));
        syscall::print("\r\n");
        // Back to sleep on the mailbox condvar — silent from here on.
    }
}

extern "C" fn condvar_beta() -> ! {
    syscall::register("beta");
    let mut message = ipc::Message::new(0, b"");

    loop {
        syscall::receive(&mut message);
        // Reaching this print means a foreign wakeup hit beta: TEST FAIL.
        syscall::print("CONDVAR beta RECV (TEST FAIL)\r\n");
    }
}

extern "C" fn condvar_probe() -> ! {
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

    syscall::print("CONDVAR TEST: send WAKE to alpha only\r\n");
    syscall::send(alpha, &ipc::Message::new(syscall::task_id(), b"WAKE"));

    // If beta ever prints, it happens within this window.
    syscall::sleep(500);
    syscall::print("CONDVAR TEST: beta silent = PASS\r\n");

    loop {
        syscall::sleep(1000);
    }
}

/// Bounded channel demo: capacity 2, fast producer, slow consumer. The
/// producer must block in `send` once two items are in flight — watch the
/// log: TX can never lead RX by more than 2.
extern "C" fn channel_producer() -> ! {
    loop {
        for item in 1..=5_u64 {
            syscall::channel_send(item);
            syscall::print("CHAN TX ");
            syscall::print_dec_u64(item);
            syscall::print("\r\n");
        }
    }
}

extern "C" fn channel_consumer() -> ! {
    loop {
        // Deliberately slow: this is what pushes the producer into the cap.
        syscall::sleep(400);
        let item = syscall::channel_receive();
        syscall::print("CHAN RX ");
        syscall::print_dec_u64(item);
        syscall::print("\r\n");
    }
}

// Per-task address spaces demo: both tasks write to the SAME virtual address
// (PRIVATE_WINDOW_VA), but each task's L0 maps it to its own private physical
// page. The write -> sleep -> read pattern is the isolation proof: without
// per-task translation, task B would overwrite the cell while A sleeps and A
// would read back 0xBBBB instead of its own 0xAAAA.
extern "C" fn vm_task_a() -> ! {
    let cell = mmu::PRIVATE_WINDOW_VA as *mut u64;
    loop {
        unsafe { core::ptr::write_volatile(cell, 0xAAAA) };
        syscall::sleep(500);
        let observed = unsafe { core::ptr::read_volatile(cell) };
        syscall::print("VM A [0x");
        syscall::print_hex_u64(mmu::PRIVATE_WINDOW_VA as u64);
        syscall::print("]=0x");
        syscall::print_hex_u64(observed);
        syscall::print("\r\n");
    }
}

extern "C" fn vm_task_b() -> ! {
    let cell = mmu::PRIVATE_WINDOW_VA as *mut u64;
    loop {
        unsafe { core::ptr::write_volatile(cell, 0xBBBB) };
        syscall::sleep(500);
        let observed = unsafe { core::ptr::read_volatile(cell) };
        syscall::print("VM B [0x");
        syscall::print_hex_u64(mmu::PRIVATE_WINDOW_VA as u64);
        syscall::print("]=0x");
        syscall::print_hex_u64(observed);
        syscall::print("\r\n");
    }
}

/// Negative isolation test: this high address is present in the kernel's
/// TTBR1 map but deliberately absent from every user TTBR0. The read must
/// fault at EL0; the exception handler kills only this task.
extern "C" fn isolation_probe() -> ! {
    syscall::print("ISOLATION PROBE: reading high-half kernel memory\r\n");
    let kernel_memory = crate::memory::phys_to_virt(crate::memory::DRAM_BASE) as *const u64;
    let value = unsafe { core::ptr::read_volatile(kernel_memory) };

    // Reaching this line means isolation is broken.
    syscall::print("ISOLATION FAIL: kernel read returned 0x");
    syscall::print_hex_u64(value);
    syscall::print("\r\n");
    loop {
        syscall::sleep(1000);
    }
}
