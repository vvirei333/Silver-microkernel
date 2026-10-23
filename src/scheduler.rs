use core::cell::UnsafeCell;

use crate::{
    interrupts,
    memory::MemoryRegion,
    pl011,
};

const MAX_TASKS: usize = 2;
const TASK_STACK_SIZE: usize = 16 * 1024;
const X30_INDEX: usize = 11;
const STACK_POINTER_INDEX: usize = 12;

#[derive(Copy, Clone)]
#[repr(C, align(16))]
struct TaskStack([u8; TASK_STACK_SIZE]);

const EMPTY_STACK: TaskStack = TaskStack([0; TASK_STACK_SIZE]);
static mut TASK_STACKS: [TaskStack; MAX_TASKS] = [EMPTY_STACK; MAX_TASKS];

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Ready,
    Running,
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

struct SchedulerState {
    tasks: [Option<Task>; MAX_TASKS],
    current: usize,
}

impl SchedulerState {
    const fn empty() -> Self {
        Self {
            tasks: [None, None],
            current: 0,
        }
    }
}

struct SharedScheduler(UnsafeCell<SchedulerState>);

// The IRQ handler runs with IRQ masking, so scheduler state cannot be re-entered
// from a timer interrupt. Add a lock when other interrupt handlers allocate or
// when SMP support appears.
unsafe impl Sync for SharedScheduler {}

static SCHEDULER: SharedScheduler = SharedScheduler(UnsafeCell::new(SchedulerState::empty()));
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
    let state = scheduler_state();
    state.tasks[0] = Some(Task::new(1, 0, task_a));
    state.tasks[1] = Some(Task::new(2, 1, task_b));
    state.current = 0;
}

/// Switches from the boot context into the first task. This function never returns.
pub fn start() -> ! {
    let state = scheduler_state();
    let Some(first_task) = state.tasks[state.current].as_mut() else {
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

    loop {
        interrupts::wait_for_irq();
    }
}

/// Selects the next ready task and switches to it.
///
/// When the current task is selected again later, execution resumes immediately
/// after the context switch call below.
pub fn schedule() {
    let state = scheduler_state();
    let current_index = state.current;
    let next_index = (current_index + 1) % MAX_TASKS;

    if state.tasks[current_index].is_none() || state.tasks[next_index].is_none() {
        return;
    }

    let Some(current_task) = state.tasks[current_index].as_mut() else {
        return;
    };
    let Some(next_task) = state.tasks[next_index].as_mut() else {
        return;
    };

    current_task.state = TaskState::Ready;
    next_task.state = TaskState::Running;
    state.current = next_index;

    let current_context = &mut current_task.context as *mut TaskContext;
    let next_context = &next_task.context as *const TaskContext;

    unsafe {
        silver_context_switch(current_context, next_context);
    }
}

impl Task {
    fn new(id: u64, stack_index: usize, entry: extern "C" fn() -> !) -> Self {
        let stack = task_stack(stack_index);
        let mut context = TaskContext::zero();
        context.registers[X30_INDEX] = entry as usize as u64;
        context.stack_pointer = stack.end as u64;

        Self {
            id,
            state: TaskState::Ready,
            context,
            stack,
        }
    }
}

fn scheduler_state() -> &'static mut SchedulerState {
    unsafe { &mut *SCHEDULER.0.get() }
}

fn task_stack(index: usize) -> MemoryRegion {
    let start = unsafe { core::ptr::addr_of!(TASK_STACKS[index]) as usize };
    MemoryRegion::new(start, start + TASK_STACK_SIZE)
}

extern "C" fn task_a() -> ! {
    loop {
        pl011::write_byte(b'A');
        interrupts::enable_irq();
        interrupts::wait_for_irq();
    }
}

extern "C" fn task_b() -> ! {
    loop {
        pl011::write_byte(b'B');
        interrupts::enable_irq();
        interrupts::wait_for_irq();
    }
}
