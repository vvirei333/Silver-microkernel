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

const SYSCALL_ERROR: u64 = u64::MAX;
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

pub fn sleep(milliseconds: u64) {
    unsafe { syscall1(SYS_SLEEP, milliseconds) };
}

pub fn send(target: u64, message: &ipc::Message) {
    unsafe { syscall2(SYS_SEND, target, message as *const ipc::Message as u64) };
}

pub fn receive(buffer: &mut ipc::Message) {
    unsafe { syscall1(SYS_RECEIVE, buffer as *mut ipc::Message as u64) };
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

pub fn register(name: &str) -> bool {
    if name.len() > NAME_LIMIT {
        return false;
    }
    with_nul(name, |ptr| unsafe { syscall1(SYS_REGISTER, ptr) }) == 0
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
