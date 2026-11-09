//! Linux signal bookkeeping and delivery.
//!
//! This module records per-task dispositions, blocked masks and pending
//! signals, applies the default actions that matter for a console OS, and
//! builds a real AArch64 `rt_sigframe` for user handlers. The handler returns
//! through the restorer stored by `rt_sigaction` (glibc's `__restore_rt`),
//! which issues `rt_sigreturn`; [`restore_frame`] then puts the interrupted
//! register bank back.
//!
//! Layout follows the kernel UAPI: `siginfo` (128 bytes) followed by
//! `ucontext` (4544 bytes), with the `sigcontext` inside the ucontext. The
//! frame is written with `copy_to_user`, so an invalid stack turns into a
//! clean process exit instead of a kernel fault.

use alloc::vec::Vec;

use crate::mutex::Mutex;
use crate::{mmu, scheduler, user_memory};

pub const SIGCHLD: u32 = 17;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;

/// One kernel `struct sigaction`: handler, flags, restorer, mask.
#[derive(Copy, Clone, Default)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

struct Table {
    task: u64,
    actions: Vec<Option<Action>>,
    blocked: u64,
    pending: u64,
    /// Registered `sigaltstack` (sp, size), if any.
    altstack: Option<(u64, u64)>,
    /// True while a handler is running on the alternate stack, so a nested
    /// signal falls back to the normal stack instead of overwriting it.
    on_altstack: bool,
}

static TABLES: Mutex<Vec<Table>> = Mutex::new(Vec::new());

/// `SIG_DFL` and `SIG_IGN` sentinels, as Linux defines them.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_ONSTACK: u64 = 0x0800_0000;

/// Frame layout in the user stack, from the kernel UAPI:
/// `struct rt_sigframe { struct siginfo info; struct ucontext uc; }`.
const SIGINFO_SIZE: usize = 128;
const UCONTEXT_SIZE: usize = 4544;
const SIGFRAME_SIZE: usize = SIGINFO_SIZE + UCONTEXT_SIZE;
/// Offsets inside the ucontext.
const UC_SIGMASK: usize = 40;
const UC_MCONTEXT: usize = 168;
const MC_REGS: usize = UC_MCONTEXT + 8; // fault_address is first
const MC_SP: usize = MC_REGS + 31 * 8;
const MC_PC: usize = MC_SP + 8;
const MC_PSTATE: usize = MC_PC + 8;
/// Offsets inside the whole rt_sigframe.
const FRAME_REGS: usize = SIGINFO_SIZE + MC_REGS;
const FRAME_SP: usize = SIGINFO_SIZE + MC_SP;
const FRAME_PC: usize = SIGINFO_SIZE + MC_PC;
const FRAME_PSTATE: usize = SIGINFO_SIZE + MC_PSTATE;

fn with_table<R>(task: u64, action: impl FnOnce(&mut Table) -> R) -> R {
    let mut tables = TABLES.lock();
    let index = match tables.iter().position(|table| table.task == task) {
        Some(index) => index,
        None => {
            let mut actions = Vec::with_capacity(65);
            actions.resize(65, None);
            tables.push(Table {
                task,
                actions,
                blocked: 0,
                pending: 0,
                altstack: None,
                on_altstack: false,
            });
            tables.len() - 1
        }
    };
    action(&mut tables[index])
}

fn bit(signal: u32) -> Option<u64> {
    if (1..=64).contains(&signal) {
        Some(1 << (signal - 1))
    } else {
        None
    }
}

pub fn action(task: u64, signal: u32) -> Option<Action> {
    bit(signal)?;
    with_table(task, |table| table.actions[signal as usize])
}

pub fn set_action(task: u64, signal: u32, action: Action) {
    if bit(signal).is_none() {
        return;
    }
    with_table(task, |table| {
        table.actions[signal as usize] = Some(action);
    });
}

pub fn blocked(task: u64) -> u64 {
    with_table(task, |table| table.blocked)
}

pub fn set_blocked(task: u64, mask: u64) {
    with_table(task, |table| table.blocked = mask);
}

/// Records a pending signal. Returns `false` for an invalid number or a
/// disposition that ignores the signal (Linux does not queue those).
pub fn raise(task: u64, signal: u32) -> bool {
    let Some(bit) = bit(signal) else {
        return false;
    };
    with_table(task, |table| {
        if table.actions[signal as usize]
            .map(|action| action.handler == SIG_IGN)
            .unwrap_or(false)
        {
            return false;
        }
        table.pending |= bit;
        true
    })
}

pub fn clear_pending(task: u64, signal: u32) {
    if let Some(bit) = bit(signal) {
        with_table(task, |table| table.pending &= !bit);
    }
}

pub fn pending(task: u64) -> u64 {
    with_table(task, |table| table.pending)
}

pub fn altstack(task: u64) -> Option<(u64, u64)> {
    with_table(task, |table| table.altstack)
}

pub fn set_altstack(task: u64, stack: Option<(u64, u64)>) {
    with_table(task, |table| {
        table.altstack = stack;
        if stack.is_none() {
            table.on_altstack = false;
        }
    });
}

pub fn on_altstack(task: u64) -> bool {
    with_table(task, |table| table.on_altstack)
}

fn set_on_altstack(task: u64, active: bool) {
    with_table(task, |table| table.on_altstack = active);
}

pub fn close_task(task: u64) {
    let mut tables = TABLES.lock();
    if let Some(index) = tables.iter().position(|table| table.task == task) {
        tables.remove(index);
    }
}

/// True when the task ever set a disposition or mask. Used to avoid creating a
/// Linux signal table for a native parent that simply forked a Linux child.
pub fn has_table(task: u64) -> bool {
    TABLES.lock().iter().any(|table| table.task == task)
}

/// Default action for a signal delivered with `SIG_DFL`. Signals whose default
/// is "ignore" or "stop" are not termination.
pub fn default_terminates(signal: u32) -> bool {
    !matches!(signal, SIGCHLD | 18 | 19 | 20 | 21 | 22 | 23 | 28)
}

fn read_user_sp() -> u64 {
    let sp: u64;
    unsafe {
        core::arch::asm!("mrs {}, sp_el0", out(reg) sp, options(nostack, preserves_flags));
    }
    sp
}

fn write_user_sp(sp: u64) {
    unsafe {
        core::arch::asm!("msr sp_el0, {}", in(reg) sp, options(nostack, preserves_flags));
    }
}

fn terminate(signal: u32) -> ! {
    scheduler::exit_current_with_status(128 + signal as u64)
}

/// Builds and enters one pending user handler, if any. Runs immediately
/// before the SVC trampoline returns to EL0, so the target is always the
/// current task and its saved register frame is available.
///
/// Ignored/default signals are cleared while scanning; the first real handler
/// wins, matching the one-signal-per-return behaviour of the kernel. A
/// default-terminating signal exits the task, and a handler without a
/// restorer falls back to the default action rather than returning into an
/// unknown address.
pub fn deliver_pending(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) {
    let task = scheduler::current_task_id();
    let blocked = blocked(task);
    let deliverable = pending(task) & !blocked;
    if deliverable == 0 {
        return;
    }

    for signal in 1..=64u32 {
        let Some(signal_bit) = bit(signal) else {
            continue;
        };
        if deliverable & signal_bit == 0 {
            continue;
        }
        let action = action(task, signal).unwrap_or_default();
        if action.handler == SIG_IGN {
            clear_pending(task, signal);
            continue;
        }
        if action.handler == SIG_DFL {
            clear_pending(task, signal);
            if default_terminates(signal) {
                terminate(signal);
            }
            continue;
        }
        // glibc does not pass SA_RESTORER on AArch64; the kernel supplies a
        // user trampoline (`mov x8, #139; svc #0`) just like the sigpage.
        let restorer = if action.flags & SA_RESTORER != 0 && action.restorer > 1 {
            action.restorer
        } else {
            match scheduler::signal_trampoline() {
                Some(trampoline) => trampoline as u64,
                None => {
                    clear_pending(task, signal);
                    if default_terminates(signal) {
                        terminate(signal);
                    }
                    continue;
                }
            }
        };

        let user_sp = read_user_sp();
        let alt = if action.flags & SA_ONSTACK != 0 && !on_altstack(task) {
            altstack(task).filter(|(sp, size)| {
                (*size >= SIGFRAME_SIZE as u64) && sp.checked_add(*size).is_some()
            })
        } else {
            None
        };
        let (stack_top, stack_floor, using_alt) = match alt {
            Some((sp, size)) => (sp + size, sp, true),
            None => (
                (mmu::USER_STACK_VA + mmu::USER_STACK_SIZE) as u64,
                mmu::USER_STACK_VA as u64,
                false,
            ),
        };
        let frame_top = if using_alt { stack_top } else { user_sp };
        let Some(signal_sp) = frame_top.checked_sub(SIGFRAME_SIZE as u64) else {
            terminate(SIGSEGV);
        };
        let signal_sp = signal_sp & !15;
        let Some(end) = signal_sp.checked_add(SIGFRAME_SIZE as u64) else {
            terminate(SIGSEGV);
        };
        if signal_sp < stack_floor || end > stack_top {
            // A frame below the stack window means the stack overran its
            // guard; report the classic SIGSEGV instead of corrupting memory.
            terminate(SIGSEGV);
        }

        let mut block = alloc::vec![0u8; SIGFRAME_SIZE];
        // siginfo: signo, errno, code (SI_USER = 0); the union stays zero.
        block[0..4].copy_from_slice(&(signal as i32).to_le_bytes());
        block[8..12].copy_from_slice(&0i32.to_le_bytes());
        // ucontext: uc_flags/uc_link/uc_stack stay zero; save the old mask.
        block[SIGINFO_SIZE + UC_SIGMASK..SIGINFO_SIZE + UC_SIGMASK + 8]
            .copy_from_slice(&blocked.to_le_bytes());
        // sigcontext: fault_address zero, all 31 registers, sp, pc, pstate.
        for (index, value) in frame[..31].iter().enumerate() {
            let slot = FRAME_REGS + index * 8;
            block[slot..slot + 8].copy_from_slice(&value.to_le_bytes());
        }
        block[FRAME_SP..FRAME_SP + 8].copy_from_slice(&user_sp.to_le_bytes());
        block[FRAME_PC..FRAME_PC + 8].copy_from_slice(&frame[31].to_le_bytes());
        block[FRAME_PSTATE..FRAME_PSTATE + 8].copy_from_slice(&frame[32].to_le_bytes());

        if user_memory::copy_to_user(&block, signal_sp).is_err() {
            clear_pending(task, signal);
            terminate(SIGSEGV);
        }
        if using_alt {
            set_on_altstack(task, true);
        }

        // Enter the handler with (sig, siginfo, ucontext) and a restorer in
        // LR. The saved frame above is restored by `rt_sigreturn`.
        frame[0] = signal as u64;
        frame[1] = signal_sp;
        frame[2] = signal_sp + SIGINFO_SIZE as u64;
        frame[30] = restorer;
        frame[31] = action.handler;
        write_user_sp(signal_sp);

        let mut mask = blocked | action.mask | signal_bit;
        if action.flags & SA_NODEFER != 0 {
            mask &= !signal_bit;
        }
        set_blocked(task, mask);
        if action.flags & SA_RESETHAND != 0 {
            set_action(task, signal, Action::default());
        }
        clear_pending(task, signal);
        return;
    }
}

/// `rt_sigreturn(2)`: the restorer calls this with SP still pointing at the
/// `rt_sigframe` that `deliver_pending` wrote.
pub fn restore_frame(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) {
    let signal_sp = read_user_sp();
    let Some(uc) = signal_sp.checked_add(SIGINFO_SIZE as u64) else {
        terminate(SIGSEGV);
    };
    let mut bytes = alloc::vec![0u8; UCONTEXT_SIZE];
    if user_memory::copy_from_user(uc, &mut bytes).is_err() {
        terminate(SIGSEGV);
    }

    let mask = u64::from_le_bytes(bytes[UC_SIGMASK..UC_SIGMASK + 8].try_into().unwrap());
    for index in 0..31 {
        let slot = MC_REGS + index * 8;
        frame[index] = u64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap());
    }
    let sp = u64::from_le_bytes(bytes[MC_SP..MC_SP + 8].try_into().unwrap());
    let pc = u64::from_le_bytes(bytes[MC_PC..MC_PC + 8].try_into().unwrap());
    let pstate = u64::from_le_bytes(bytes[MC_PSTATE..MC_PSTATE + 8].try_into().unwrap());

    frame[31] = pc;
    frame[32] = pstate;
    write_user_sp(sp);
    let task = scheduler::current_task_id();
    set_blocked(task, mask);
    set_on_altstack(task, false);
}
