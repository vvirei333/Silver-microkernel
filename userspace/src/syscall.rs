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
const SYS_TASK_LIST: u64 = 22;
const SYS_SPAWN_ARGS: u64 = 23;
const SYS_ARGS: u64 = 24;
const SYS_WAIT: u64 = 25;
const SYS_KILL: u64 = 26;
const SYS_FS_OPEN: u64 = 27;
const SYS_FS_READ: u64 = 28;
const SYS_FS_WRITE: u64 = 29;
const SYS_FS_CREATE: u64 = 30;
const SYS_FS_CLOSE: u64 = 31;
const SYS_FS_READ_DIR: u64 = 32;
const SYS_FS_MKDIR: u64 = 33;
const SYS_CHDIR: u64 = 34;
const SYS_GETCWD: u64 = 35;
const SYS_FS_UNLINK: u64 = 36;
const SYS_FS_RMDIR: u64 = 37;
const SYS_FS_RENAME: u64 = 38;
const SYS_CONSOLE_SET: u64 = 39;
const SYS_TASK_RESUME: u64 = 40;
const SYS_WAIT_EVENT: u64 = 41;
const SYS_CAP_GET_TASK_ID: u64 = 60;
const SYS_CAP_GRANT: u64 = 61;
const SYS_MAP_PHYS: u64 = 62;
const SYS_BIND_IRQ: u64 = 63;
const SYS_WAIT_IRQ: u64 = 64;
const SYS_TRY_SEND: u64 = 65;

pub const USER_HEAP_VA: usize = 0x4020_0000;
pub const USER_HEAP_MAX_SIZE: usize = 0x20_0000;
pub const PROGRAM_DEMO: u64 = 0;
pub const PROGRAM_NAMESERVER: u64 = 1;
pub const PROGRAM_SILVERSHELL: u64 = 2;
pub const PROGRAM_LS: u64 = 3;
pub const PROGRAM_CAT: u64 = 4;
pub const PROGRAM_ECHO: u64 = 5;
pub const PROGRAM_SAY: u64 = 6;
pub const PROGRAM_SL: u64 = 7;
pub const PROGRAM_UART_DRIVER: u64 = 8;
pub const OPEN_READ: u64 = 0;
pub const OPEN_WRITE: u64 = 1;
pub const OPEN_APPEND: u64 = 2;
pub const DIR_NAME_CAPACITY: usize = 32;
pub const PATH_LIMIT: usize = 191;
const SYSCALL_ERROR: u64 = u64::MAX;
const SYSCALL_TIMEOUT: u64 = u64::MAX - 1;
const SYSCALL_STOPPED: u64 = u64::MAX - 2;
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct TaskInfo {
    pub id: u64,
    pub state: u64,
    pub program: u64,
    pub argument: u64,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct DirEntry {
    pub name: [u8; DIR_NAME_CAPACITY],
    pub name_len: u64,
    pub kind: u64,
}

impl DirEntry {
    pub fn name(&self) -> Option<&str> {
        let len = usize::try_from(self.name_len).ok()?;
        core::str::from_utf8(self.name.get(..len)?).ok()
    }

    pub fn is_directory(&self) -> bool {
        self.kind == 2
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Timeout,
    Failed,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitEvent {
    Exited(u64),
    Stopped,
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

pub fn task_list(buffer: &mut [TaskInfo]) -> Option<usize> {
    match unsafe {
        syscall2(
            SYS_TASK_LIST,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    } {
        SYSCALL_ERROR => None,
        count => Some(count as usize),
    }
}

pub fn send(target: u64, message: &ipc::Message) -> bool {
    unsafe { syscall2(SYS_SEND, target, message as *const ipc::Message as u64) == 0 }
}

pub fn try_send(target: u64, message: &ipc::Message) -> bool {
    unsafe { syscall2(SYS_TRY_SEND, target, message as *const ipc::Message as u64) == 0 }
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
    print_bytes(text.as_bytes());
}

pub fn print_bytes(bytes: &[u8]) {
    unsafe { syscall2(SYS_PRINT, bytes.as_ptr() as u64, bytes.len() as u64) };
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

pub fn mutex_unlock() -> bool {
    unsafe { syscall0(SYS_MUTEX_UNLOCK) == 0 }
}

pub fn channel_send(value: u64) {
    unsafe { syscall1(SYS_CHAN_SEND, value) };
}

pub fn channel_receive() -> u64 {
    unsafe { syscall0(SYS_CHAN_RECV) }
}

pub fn spawn(program: u64, argument: u64) -> Option<u64> {
    let cap_id = unsafe { syscall2(SYS_SPAWN, program, argument) };
    optional_id(cap_id)
}

pub fn cap_get_task_id(cap_id: u64) -> Option<u64> {
    let task_id = unsafe { syscall1(SYS_CAP_GET_TASK_ID, cap_id) };
    optional_id(task_id)
}

pub fn cap_grant(cap_id: u64, recipient_task_id: u64, rights: u32) -> bool {
    unsafe { syscall3(SYS_CAP_GRANT, cap_id, recipient_task_id, rights as u64) == 0 }
}

pub fn map_phys(phys_addr: u64, size: u64) -> Option<u64> {
    optional_id(unsafe { syscall2(SYS_MAP_PHYS, phys_addr, size) })
}

pub fn bind_irq(intid: u64) -> bool {
    unsafe { syscall1(SYS_BIND_IRQ, intid) == 0 }
}

pub fn wait_irq() {
    unsafe { syscall0(SYS_WAIT_IRQ) };
}

pub fn spawn_with_args(program: u64, argument: u64, arguments: &str) -> Option<u64> {
    if arguments.len() > 192 {
        return None;
    }
    let cap_id = unsafe {
        syscall4(
            SYS_SPAWN_ARGS,
            program,
            argument,
            arguments.as_ptr() as u64,
            arguments.len() as u64,
        )
    };
    optional_id(cap_id)
}

pub fn arguments(buffer: &mut [u8]) -> Option<usize> {
    match unsafe { syscall2(SYS_ARGS, buffer.as_mut_ptr() as u64, buffer.len() as u64) } {
        SYSCALL_ERROR => None,
        len => Some(len as usize),
    }
}

pub fn wait(cap_id: u64) -> Option<u64> {
    match unsafe { syscall1(SYS_WAIT, cap_id) } {
        SYSCALL_ERROR => None,
        status => Some(status),
    }
}

pub fn wait_event(cap_id: u64) -> Option<WaitEvent> {
    match unsafe { syscall1(SYS_WAIT_EVENT, cap_id) } {
        SYSCALL_ERROR => None,
        SYSCALL_STOPPED => Some(WaitEvent::Stopped),
        status => Some(WaitEvent::Exited(status)),
    }
}

pub fn kill(cap_id: u64) -> bool {
    unsafe { syscall1(SYS_KILL, cap_id) == 0 }
}

pub fn console_set(cap_id: u64) -> bool {
    unsafe { syscall1(SYS_CONSOLE_SET, cap_id) == 0 }
}

pub fn resume(cap_id: u64) -> bool {
    unsafe { syscall1(SYS_TASK_RESUME, cap_id) == 0 }
}

pub fn exit() -> ! {
    exit_with_status(0)
}

pub fn exit_with_status(status: u64) -> ! {
    unsafe { syscall1(SYS_EXIT, status) };
    loop {
        core::hint::spin_loop();
    }
}

fn optional_id(id: u64) -> Option<u64> {
    if id == SYSCALL_ERROR {
        None
    } else {
        Some(id)
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

pub fn fs_open(path: &str, flags: u64) -> Option<u64> {
    let result = with_path(path, |ptr| unsafe { syscall2(SYS_FS_OPEN, ptr, flags) });
    optional_id(result)
}

pub fn fs_create(path: &str) -> bool {
    with_path(path, |ptr| unsafe { syscall1(SYS_FS_CREATE, ptr) }) == 0
}

pub fn fs_mkdir(path: &str) -> bool {
    with_path(path, |ptr| unsafe { syscall1(SYS_FS_MKDIR, ptr) }) == 0
}

pub fn fs_unlink(path: &str) -> bool {
    with_path(path, |ptr| unsafe { syscall1(SYS_FS_UNLINK, ptr) }) == 0
}

pub fn fs_rmdir(path: &str) -> bool {
    with_path(path, |ptr| unsafe { syscall1(SYS_FS_RMDIR, ptr) }) == 0
}

pub fn fs_rename(source: &str, destination: &str) -> bool {
    with_path(source, |source_ptr| {
        with_path(destination, |destination_ptr| unsafe {
            syscall2(SYS_FS_RENAME, source_ptr, destination_ptr)
        })
    }) == 0
}

pub fn fs_close(fd: u64) -> bool {
    unsafe { syscall1(SYS_FS_CLOSE, fd) == 0 }
}

pub fn fs_read(fd: u64, buffer: &mut [u8]) -> Option<usize> {
    match unsafe {
        syscall3(
            SYS_FS_READ,
            fd,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    } {
        SYSCALL_ERROR => None,
        count => Some(count as usize),
    }
}

pub fn fs_write(fd: u64, bytes: &[u8]) -> Option<usize> {
    match unsafe { syscall3(SYS_FS_WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) } {
        SYSCALL_ERROR => None,
        count => Some(count as usize),
    }
}

pub fn fs_read_dir(path: &str, entries: &mut [DirEntry]) -> Option<usize> {
    let result = with_path(path, |ptr| unsafe {
        syscall3(
            SYS_FS_READ_DIR,
            ptr,
            entries.as_mut_ptr() as u64,
            entries.len() as u64,
        )
    });
    match result {
        SYSCALL_ERROR => None,
        count => Some(count as usize),
    }
}

pub fn fs_chdir(path: &str) -> bool {
    with_path(path, |ptr| unsafe { syscall1(SYS_CHDIR, ptr) }) == 0
}

pub fn fs_getcwd(buffer: &mut [u8]) -> Option<usize> {
    match unsafe { syscall2(SYS_GETCWD, buffer.as_mut_ptr() as u64, buffer.len() as u64) } {
        SYSCALL_ERROR => None,
        len => Some(len as usize),
    }
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

fn with_path<R>(path: &str, call: impl FnOnce(u64) -> R) -> R {
    let mut buffer = [0_u8; PATH_LIMIT + 1];
    if path.len() > PATH_LIMIT {
        return call(u64::MAX);
    }
    buffer[..path.len()].copy_from_slice(path.as_bytes());
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

unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        inlateout("x0") arg0 => result,
        in("x1") arg1,
        in("x2") arg2,
        options(nostack),
    );
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    result
}

unsafe fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    core::arch::asm!(
        "svc #0",
        in("x8") number,
        inlateout("x0") arg0 => result,
        in("x1") arg1,
        in("x2") arg2,
        in("x3") arg3,
        options(nostack),
    );
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    result
}
