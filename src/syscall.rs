//! System call interface: tasks request kernel services through `svc #0`.
//!
//! Calling convention: call number in x8, arguments in x0-x5, result in x0.
//! The SVC trampoline (exception_svc_dispatch in main.rs) saves the full
//! register bank plus ELR_EL1/SPSR_EL1 on the kernel stack before dispatch.
//! Keeping ELR/SPSR in the *stack frame* is essential: syscalls like
//! receive() block and switch to another task, and other tasks take
//! exceptions that overwrite the CPU-global ELR_EL1/SPSR_EL1 registers.
//! The per-task stack copy is what makes eret land back in the right task.

use alloc::vec::Vec;

use crate::{channel, ipc, memory, mutex, pl011, scheduler};

pub const SYS_SLEEP: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECEIVE: u64 = 3;
pub const SYS_REGISTER: u64 = 4;
pub const SYS_LOOKUP: u64 = 5;
pub const SYS_PRINT: u64 = 6;
pub const SYS_WHOAMI: u64 = 7;
pub const SYS_PRINT_HEX: u64 = 8;
pub const SYS_PRINT_DEC: u64 = 9;
pub const SYS_MUTEX_LOCK: u64 = 10;
pub const SYS_MUTEX_UNLOCK: u64 = 11;
pub const SYS_CHAN_SEND: u64 = 12;
pub const SYS_CHAN_RECV: u64 = 13;

/// Well-known task ID of the nameserver. Fixed by spawn order: the scheduler
/// spawns it first, right after idle (which is always ID 0).
pub const NAMESERVER_ID: u64 = 1;

/// Longest name the nameserver protocol carries: payload is 32 bytes, two go
/// to the "R "/"L " prefix.
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

/// Written into x0 when the call number is not in the table.
pub const SYSCALL_ERROR: u64 = u64::MAX;

/// Frame layout built by the SVC trampoline: x0-x30, then ELR, then SPSR.
const FRAME_QUADWORDS: usize = 33;

/// Kernel objects the demo tasks share. They live behind the syscall
/// boundary: user tasks hold the demo mutex across several syscalls, so
/// the lock is raw (no guard can survive the SVC round trips).
static DEMO_MUTEX: mutex::Mutex<()> = mutex::Mutex::new(());
static DEMO_CHANNEL: channel::Channel<u64> = channel::Channel::new(2);

/// A task-supplied buffer may live anywhere in DRAM (task stacks, user
/// memory, statics) but never in the private VA window or MMIO: those VAs
/// are not mapped in the kernel's own tables, so the kernel cannot touch
/// them. Reject anything outside the DRAM identity range.
fn valid_task_slice(ptr: u64, len: u64) -> bool {
    let base = memory::DRAM_BASE as u64;
    let end = memory::DRAM_END as u64;
    len <= 4096 && ptr >= base && ptr.checked_add(len).is_some_and(|last| last <= end)
}

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
        SYS_REGISTER => sys_register_name(frame[0]),
        SYS_LOOKUP => sys_lookup_name(frame[0]),
        SYS_PRINT => sys_print(frame[0], frame[1]),
        SYS_WHOAMI => scheduler::current_task_id(),
        SYS_PRINT_HEX => {
            pl011::write_hex_u64(frame[0]);
            0
        }
        SYS_PRINT_DEC => {
            pl011::write_dec_u64(frame[0]);
            0
        }
        SYS_MUTEX_LOCK => {
            DEMO_MUTEX.raw_lock();
            0
        }
        SYS_MUTEX_UNLOCK => {
            DEMO_MUTEX.raw_unlock();
            0
        }
        SYS_CHAN_SEND => {
            DEMO_CHANNEL.send(frame[0]);
            0
        }
        SYS_CHAN_RECV => DEMO_CHANNEL.receive(),
        _ => SYSCALL_ERROR,
    };

    frame[0] = result;
}

fn sys_print(ptr: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    if !valid_task_slice(ptr, len) {
        return SYSCALL_ERROR;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    pl011::write_bytes(bytes);
    0
}

fn sys_send(target: u64, message_ptr: u64) -> u64 {
    let size = core::mem::size_of::<ipc::Message>() as u64;
    if !valid_task_slice(message_ptr, size) {
        return SYSCALL_ERROR;
    }
    let message = unsafe { core::ptr::read(message_ptr as *const ipc::Message) };
    ipc::send(target, message);
    0
}

fn sys_receive(buffer_ptr: u64) -> u64 {
    let size = core::mem::size_of::<ipc::Message>() as u64;
    if !valid_task_slice(buffer_ptr, size) {
        return SYSCALL_ERROR;
    }
    let message = ipc::receive();
    unsafe { core::ptr::write(buffer_ptr as *mut ipc::Message, message) };
    0
}

// Nameserver RPC. These syscalls do a full request/reply round trip over
// IPC: build a text request, send it to NAMESERVER_ID, block in receive()
// for the reply. The nameserver itself never calls these.

fn sys_register_name(name_ptr: u64) -> u64 {
    let Some(name) = (unsafe { read_name(name_ptr) }) else {
        return SYSCALL_ERROR;
    };
    let Some(request) = build_request(b"R ", &name) else {
        return SYSCALL_ERROR;
    };

    ipc::send(NAMESERVER_ID, request);
    let reply = ipc::receive();
    if reply.text() == &b"OK"[..] {
        0
    } else {
        SYSCALL_ERROR
    }
}

fn sys_lookup_name(name_ptr: u64) -> u64 {
    let Some(name) = (unsafe { read_name(name_ptr) }) else {
        return 0;
    };
    let Some(request) = build_request(b"L ", &name) else {
        return 0;
    };

    ipc::send(NAMESERVER_ID, request);
    let reply = ipc::receive();
    match parse_id_reply(&reply) {
        Some(id) => id,
        None => 0,
    }
}

/// Reads a NUL-terminated name from task memory, capped at NAME_LIMIT bytes.
///
/// # Safety
/// The pointer must be valid for reads up to and including the NUL byte.
unsafe fn read_name(ptr: u64) -> Option<Vec<u8>> {
    if !valid_task_slice(ptr, NAME_LIMIT as u64 + 1) {
        return None;
    }

    let bytes = ptr as *const u8;
    let mut name = Vec::new();
    for offset in 0..NAME_LIMIT {
        let byte = unsafe { core::ptr::read(bytes.add(offset)) };
        if byte == 0 {
            return Some(name);
        }
        name.push(byte);
    }
    // Exactly NAME_LIMIT bytes long: valid only if the next byte is the NUL.
    if unsafe { core::ptr::read(bytes.add(NAME_LIMIT)) } == 0 {
        Some(name)
    } else {
        None
    }
}

fn build_request(prefix: &[u8], name: &[u8]) -> Option<ipc::Message> {
    if prefix.len() + name.len() > ipc::PAYLOAD_SIZE {
        return None;
    }
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[..prefix.len()].copy_from_slice(prefix);
    payload[prefix.len()..prefix.len() + name.len()].copy_from_slice(name);
    Some(ipc::Message {
        sender: scheduler::current_task_id(),
        payload,
    })
}

fn parse_id_reply(reply: &ipc::Message) -> Option<u64> {
    let digits = reply.text().strip_prefix(b"ID ")?;
    let mut id = 0_u64;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        id = id * 10 + u64::from(byte - b'0');
    }
    Some(id)
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

/// Writes `text` to the kernel console. The only legal way for an EL0 task
/// to reach the UART: MMIO is unmapped at EL0.
pub fn print(text: &str) {
    unsafe { syscall2(SYS_PRINT, text.as_ptr() as u64, text.len() as u64) };
}

/// ID of the current task.
pub fn task_id() -> u64 {
    unsafe { syscall0(SYS_WHOAMI) }
}

/// Prints `value` as 16 hex digits through the kernel console.
pub fn print_hex_u64(value: u64) {
    unsafe { syscall1(SYS_PRINT_HEX, value) };
}

/// Prints `value` in decimal through the kernel console.
pub fn print_dec_u64(value: u64) {
    unsafe { syscall1(SYS_PRINT_DEC, value) };
}

/// Acquires the demo mutex, blocking until it is free. Pair with
/// `mutex_unlock` — the lock is held across syscalls, so there is no guard.
pub fn mutex_lock() {
    unsafe { syscall0(SYS_MUTEX_LOCK) };
}

/// Releases the demo mutex.
pub fn mutex_unlock() {
    unsafe { syscall0(SYS_MUTEX_UNLOCK) };
}

/// Sends a value into the demo channel, blocking when it is full.
pub fn channel_send(value: u64) {
    unsafe { syscall1(SYS_CHAN_SEND, value) };
}

/// Receives a value from the demo channel, blocking while it is empty.
pub fn channel_receive() -> u64 {
    unsafe { syscall0(SYS_CHAN_RECV) }
}

/// Registers the current task under `name` at the nameserver.
/// Returns false if the name is taken or too long.
pub fn register(name: &str) -> bool {
    if name.len() > NAME_LIMIT {
        return false;
    }
    with_nul(name, |ptr| unsafe { syscall1(SYS_REGISTER, ptr) }) == 0
}

/// Looks up the task ID registered under `name`. None = not registered.
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

/// Copies `name` into a NUL-terminated stack buffer and passes its address
/// to `call`. The buffer outlives the syscall (the kernel copies the name
/// into the request message before returning).
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
