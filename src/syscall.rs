//! System call interface: tasks request kernel services through `svc #0`.
//!
//! Calling convention: call number in x8, arguments in x0-x5, result in x0.
//! The SVC trampoline (exception_svc_dispatch in main.rs) saves the full
//! register bank plus ELR_EL1/SPSR_EL1 on the kernel stack before dispatch.
//! Keeping ELR/SPSR in the *stack frame* is essential: syscalls like
//! receive() block and switch to another task, and other tasks take
//! exceptions that overwrite the CPU-global ELR_EL1/SPSR_EL1 registers.
//! The per-task stack copy is what makes eret land back in the right task.

use crate::{ipc, scheduler};

pub const SYS_SLEEP: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECEIVE: u64 = 3;

/// Written into x0 when the call number is not in the table.
pub const SYSCALL_ERROR: u64 = u64::MAX;

/// Frame layout built by the SVC trampoline: x0-x30, then ELR, then SPSR.
const FRAME_QUADWORDS: usize = 33;

/// Entry point for the SVC trampoline. `frame` points at the saved register
/// bank on the kernel stack; the result is written into frame[0] (x0).
#[no_mangle]
extern "C" fn silver_handle_syscall(frame: *mut u64) {
    let frame = unsafe { &mut *(frame as *mut [u64; FRAME_QUADWORDS]) };
    let number = frame[8]; // x8 carries the call number

    let result = match number {
        SYS_SLEEP => {
            scheduler::sleep(frame[0]);
            0
        }
        SYS_SEND => sys_send(frame[0], frame[1]),
        SYS_RECEIVE => sys_receive(frame[0]),
        _ => SYSCALL_ERROR,
    };

    frame[0] = result;
}

fn sys_send(target: u64, message_ptr: u64) -> u64 {
    if message_ptr == 0 {
        return SYSCALL_ERROR;
    }
    // Tasks share one address space for now, so the pointer is a plain copy.
    let message = unsafe { core::ptr::read(message_ptr as *const ipc::Message) };
    ipc::send(target, message);
    0
}

fn sys_receive(buffer_ptr: u64) -> u64 {
    if buffer_ptr == 0 {
        return SYSCALL_ERROR;
    }
    let message = ipc::receive();
    unsafe { core::ptr::write(buffer_ptr as *mut ipc::Message, message) };
    0
}

/// Blocks the current task for approximately `milliseconds`.
pub fn sleep(milliseconds: u64) {
    unsafe { syscall1(SYS_SLEEP, milliseconds) };
}

/// Sends a message to task `target`.
pub fn send(target: u64, message: &ipc::Message) {
    unsafe { syscall2(SYS_SEND, target, message as *const ipc::Message as u64) };
}

/// Blocks until a message arrives and writes it into `buffer`.
pub fn receive(buffer: &mut ipc::Message) {
    unsafe { syscall1(SYS_RECEIVE, buffer as *mut ipc::Message as u64) };
}

unsafe fn syscall1(number: u64, arg0: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        inlateout("x0") arg0 => result,
        options(nostack),
    );
    result
}

unsafe fn syscall2(number: u64, arg0: u64, arg1: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        inlateout("x0") arg0 => result,
        in("x1") arg1,
        options(nostack),
    );
    result
}
