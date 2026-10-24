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

use crate::{channel, ipc, mmu, mutex, pl011, scheduler};

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

/// Accept only pages mapped in the current task's TTBR0: its own ELF image,
/// fixed-VA stack, or private page. The kernel dereferences the pointer through
/// that same TTBR0 while TTBR1 keeps privileged state only at high VAs.
fn valid_task_slice(ptr: u64, len: u64) -> bool {
    mmu::valid_user_slice(ptr, len)
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
