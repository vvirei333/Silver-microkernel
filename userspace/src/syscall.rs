#![allow(dead_code)] // Each executable uses a subset of the shared syscall API.

use crate::ipc;

const SYS_SLEEP: u64 = 1;
const SYS_SEND: u64 = 2;
const SYS_RECEIVE: u64 = 3;
const SYS_REGISTER: u64 = 4;
const SYS_LOOKUP: u64 = 5;
const SYS_PRINT: u64 = 6;
const SYS_WHOAMI: u64 = 7;
const SYS_PRINT_HEX: u64 = 8;
const SYS_PRINT_DEC: u64 = 9;
const SYS_MUTEX_LOCK: u64 = 10;
const SYS_MUTEX_UNLOCK: u64 = 11;
const SYS_CHAN_SEND: u64 = 12;
const SYS_CHAN_RECV: u64 = 13;
const SYS_SPAWN: u64 = 14;
const SYS_EXIT: u64 = 15;
const SYS_UNREGISTER: u64 = 16;
const SYS_TASK_ALIVE: u64 = 17;
const SYS_BRK: u64 = 18;
const SYS_READ: u64 = 19;
const SYS_UPTIME: u64 = 20;
const SYS_MEMINFO: u64 = 21;

pub const USER_HEAP_VA: usize = 0x4020_0000;
pub const USER_HEAP_MAX_SIZE: usize = 0x20_0000;
pub const PROGRAM_DEMO: u64 = 0;
pub const PROGRAM_NAMESERVER: u64 = 1;
pub const PROGRAM_SILVERSHELL: u64 = 2;
const SYSCALL_ERROR: u64 = u64::MAX;
const SYSCALL_TIMEOUT: u64 = u64::MAX - 1;
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Timeout,
    Failed,
}

pub fn sleep(milliseconds: u64) {
    unsafe { syscall1(SYS_SLEEP, milliseconds) };
}

/// Non-blocking UART input. The shell sleeps briefly when no byte is available.
pub fn read() -> Option<u8> {
    match unsafe { syscall0(SYS_READ) } {
        SYSCALL_ERROR => None,
        byte => Some(byte as u8),
    }
}

pub fn uptime_ticks() -> u64 {
    unsafe { syscall0(SYS_UPTIME) }
}

pub fn memory_info() -> Option<MemoryInfo> {
    let mut info = MemoryInfo {
        total_bytes: 0,
        used_bytes: 0,
        free_bytes: 0,
    };
    if unsafe { syscall1(SYS_MEMINFO, &mut info as *mut MemoryInfo as u64) } == 0 {
        Some(info)
    } else {
        None
    }
}

pub fn send(target: u64, message: &ipc::Message) -> bool {
    unsafe { syscall2(SYS_SEND, target, message as *const ipc::Message as u64) == 0 }
}

pub fn receive(buffer: &mut ipc::Message) -> bool {
    unsafe { receive_raw(buffer as *mut ipc::Message) }
}

/// Waits up to `timeout_ticks` timer ticks. Zero performs a non-blocking poll;
/// `u64::MAX` waits indefinitely. A deadline never discards an available message.
pub fn receive_timeout(buffer: &mut ipc::Message, timeout_ticks: u64) -> Result<(), ReceiveError> {
    match unsafe {
        syscall2(
            SYS_RECEIVE,
            buffer as *mut ipc::Message as u64,
            timeout_ticks,
        )
    } {
        0 => Ok(()),
        SYSCALL_TIMEOUT => Err(ReceiveError::Timeout),
        _ => Err(ReceiveError::Failed),
    }
}

/// Raw variant used by permission tests. Returns false without blocking when
/// the kernel rejects the output buffer.
pub unsafe fn receive_raw(buffer: *mut ipc::Message) -> bool {
    unsafe { syscall2(SYS_RECEIVE, buffer as u64, u64::MAX) == 0 }
}

pub fn print(text: &str) {
    unsafe { syscall2(SYS_PRINT, text.as_ptr() as u64, text.len() as u64) };
}

pub fn task_id() -> u64 {
    unsafe { syscall0(SYS_WHOAMI) }
}

pub fn print_hex_u64(value: u64) {
    unsafe { syscall1(SYS_PRINT_HEX, value) };
}

pub fn print_dec_u64(value: u64) {
    unsafe { syscall1(SYS_PRINT_DEC, value) };
}

pub fn mutex_lock() {
    unsafe { syscall0(SYS_MUTEX_LOCK) };
}

pub fn mutex_unlock() {
    unsafe { syscall0(SYS_MUTEX_UNLOCK) };
}

pub fn channel_send(value: u64) {
    unsafe { syscall1(SYS_CHAN_SEND, value) };
}

pub fn channel_receive() -> u64 {
    unsafe { syscall0(SYS_CHAN_RECV) }
}

pub fn spawn(program: u64, argument: u64) -> Option<u64> {
    let id = unsafe { syscall2(SYS_SPAWN, program, argument) };
    if id == SYSCALL_ERROR {
        None
    } else {
        Some(id)
    }
}

pub fn exit() -> ! {
    unsafe { syscall0(SYS_EXIT) };
    loop {
        core::hint::spin_loop();
    }
}

pub fn task_alive(id: u64) -> bool {
    unsafe { syscall1(SYS_TASK_ALIVE, id) != 0 }
}

/// Returns the current byte-granular program break.
pub fn program_break() -> Option<usize> {
    let address = unsafe { syscall1(SYS_BRK, 0) };
    if address == SYSCALL_ERROR {
        None
    } else {
        Some(address as usize)
    }
}

/// Grows or shrinks the current task's heap to an absolute program break.
pub fn set_program_break(address: usize) -> bool {
    unsafe { syscall1(SYS_BRK, address as u64) == address as u64 }
}

pub fn register(name: &str) -> bool {
    if name.len() > NAME_LIMIT {
        return false;
    }
    with_nul(name, |ptr| unsafe { syscall1(SYS_REGISTER, ptr) }) == 0
}

pub fn unregister(name: &str) -> bool {
    if name.len() > NAME_LIMIT {
        return false;
    }
    with_nul(name, |ptr| unsafe { syscall1(SYS_UNREGISTER, ptr) }) == 0
}

pub fn lookup(name: &str) -> Option<u64> {
    if name.len() > NAME_LIMIT {
        return None;
    }
    let id = with_nul(name, |ptr| unsafe { syscall1(SYS_LOOKUP, ptr) });
    if id == 0 || id == SYSCALL_ERROR {
        None
    } else {
        Some(id)
    }
}

fn with_nul<R>(name: &str, call: impl FnOnce(u64) -> R) -> R {
    let mut buffer = [0_u8; NAME_LIMIT + 1];
    buffer[..name.len()].copy_from_slice(name.as_bytes());
    call(buffer.as_ptr() as u64)
}

unsafe fn syscall0(number: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        lateout("x0") result,
        options(nostack),
    );
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    result
}

unsafe fn syscall1(number: u64, arg0: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        inlateout("x0") arg0 => result,
        options(nostack),
    );
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
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
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    result
}
