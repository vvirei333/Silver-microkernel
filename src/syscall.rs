//! System call interface: tasks request kernel services through `svc #0`.
//!
//! Calling convention: call number in x8, arguments in x0-x5, result in x0.
//! SYS_ALLOC_DMA additionally returns the buffer's physical address in x1.
//! The SVC trampoline (exception_svc_dispatch in main.rs) saves the full
//! register bank plus ELR_EL1/SPSR_EL1 on the kernel stack before dispatch.
//! Keeping ELR/SPSR in the *stack frame* is essential: syscalls like
//! receive() block and switch to another task, and other tasks take
//! exceptions that overwrite the CPU-global ELR_EL1/SPSR_EL1 registers.
//! The per-task stack copy is what makes eret land back in the right task.

use alloc::{vec, vec::Vec};

use crate::{
    capability, channel, frame_allocator, ipc, memory, mmu, mutex, pl011, scheduler, timer,
    user_memory, vfs,
};

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
pub const SYS_SPAWN: u64 = 14;
pub const SYS_EXIT: u64 = 15;
pub const SYS_UNREGISTER: u64 = 16;
pub const SYS_TASK_ALIVE: u64 = 17;
pub const SYS_BRK: u64 = 18;
pub const SYS_READ: u64 = 19;
pub const SYS_UPTIME: u64 = 20;
pub const SYS_MEMINFO: u64 = 21;
pub const SYS_TASK_LIST: u64 = 22;
pub const SYS_SPAWN_ARGS: u64 = 23;
pub const SYS_ARGS: u64 = 24;
pub const SYS_WAIT: u64 = 25;
pub const SYS_KILL: u64 = 26;
pub const SYS_FS_OPEN: u64 = 27;
pub const SYS_FS_READ: u64 = 28;
pub const SYS_FS_WRITE: u64 = 29;
pub const SYS_FS_CREATE: u64 = 30;
pub const SYS_FS_CLOSE: u64 = 31;
pub const SYS_FS_READ_DIR: u64 = 32;
pub const SYS_FS_MKDIR: u64 = 33;
pub const SYS_CHDIR: u64 = 34;
pub const SYS_GETCWD: u64 = 35;
pub const SYS_FS_UNLINK: u64 = 36;
pub const SYS_FS_RMDIR: u64 = 37;
pub const SYS_FS_RENAME: u64 = 38;
pub const SYS_CONSOLE_SET: u64 = 39;
pub const SYS_TASK_RESUME: u64 = 40;
pub const SYS_WAIT_EVENT: u64 = 41;
pub const SYS_CAP_GET_TASK_ID: u64 = 60;
pub const SYS_CAP_GRANT: u64 = 61;
pub const SYS_MAP_PHYS: u64 = 62;
pub const SYS_BIND_IRQ: u64 = 63;
pub const SYS_WAIT_IRQ: u64 = 64;
pub const SYS_TRY_SEND: u64 = 65;
pub const SYS_STOP_CONSOLE: u64 = 66;
pub const SYS_ALLOC_DMA: u64 = 67;
pub const SYS_PCI_READ: u64 = 68;
pub const SYS_PCI_WRITE: u64 = 69;

const ARGUMENT_LIMIT: usize = 192;
const PATH_LIMIT: usize = 191;
const IO_LIMIT: usize = 4096;

/// Well-known Scout task ID. Demo boot starts Scout directly after idle;
/// normal boot starts silverd first and lets silverd create Scout second.
#[cfg(feature = "demo-tasks")]
pub const SCOUT_ID: u64 = 1;
#[cfg(not(feature = "demo-tasks"))]
pub const SCOUT_ID: u64 = 2;

/// Longest name the nameserver protocol carries: payload is 32 bytes, two go
/// to the "R "/"L " prefix.
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

/// Written into x0 when the call number is not in the table.
pub const SYSCALL_ERROR: u64 = u64::MAX;
pub const SYSCALL_TIMEOUT: u64 = u64::MAX - 1;
pub const SYSCALL_STOPPED: u64 = u64::MAX - 2;

/// Frame layout built by the SVC trampoline: x0-x30, then ELR, then SPSR.
const FRAME_QUADWORDS: usize = 33;

/// Stable userspace ABI for SYS_MEMINFO. All values are byte counts.
#[repr(C)]
struct MemoryInfo {
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
}

/// Kernel objects the demo tasks share. They live behind the syscall
/// boundary: user tasks hold the demo mutex across several syscalls, so
/// the lock is raw (no guard can survive the SVC round trips).
static DEMO_MUTEX: mutex::Mutex<()> = mutex::Mutex::new(());
static DEMO_CHANNEL: channel::Channel<u64> = channel::Channel::new(2);

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
        SYS_TRY_SEND => sys_try_send(frame[0], frame[1]),
        SYS_RECEIVE => sys_receive(frame[0], frame[1]),
        SYS_REGISTER => sys_register_name(frame[0]),
        SYS_LOOKUP => sys_lookup_name(frame[0]),
        SYS_PRINT => sys_print(frame[0], frame[1]),
        SYS_WHOAMI => scheduler::current_task_id(),
        SYS_PRINT_HEX => sys_print_hex(frame[0]),
        SYS_PRINT_DEC => sys_print_dec(frame[0]),
        SYS_MUTEX_LOCK => {
            DEMO_MUTEX.raw_lock();
            0
        }
        SYS_MUTEX_UNLOCK => {
            if DEMO_MUTEX.raw_unlock() {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_CHAN_SEND => {
            DEMO_CHANNEL.send(frame[0]);
            0
        }
        SYS_CHAN_RECV => DEMO_CHANNEL.receive(),
        SYS_SPAWN => scheduler::spawn_user_with_cap(frame[0], frame[1]).unwrap_or(SYSCALL_ERROR),
        SYS_EXIT => scheduler::exit_current_with_status(frame[0]),
        SYS_UNREGISTER => sys_unregister_name(frame[0]),
        SYS_TASK_ALIVE => u64::from(scheduler::task_alive(frame[0])),
        SYS_BRK => scheduler::change_program_break(frame[0] as usize)
            .map(|address| address as u64)
            .unwrap_or(SYSCALL_ERROR),
        SYS_READ => {
            if scheduler::current_task_owns_console() {
                pl011::read_byte().map(u64::from).unwrap_or(SYSCALL_ERROR)
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_UPTIME => timer::ticks(),
        SYS_MEMINFO => sys_meminfo(frame[0]),
        SYS_TASK_LIST => sys_task_list(frame[0], frame[1]),
        SYS_SPAWN_ARGS => sys_spawn_with_args(frame[0], frame[1], frame[2], frame[3]),
        SYS_ARGS => sys_arguments(frame[0], frame[1]),
        SYS_WAIT => scheduler::capability_task_id_with_rights(frame[0], capability::CAP_WAIT)
            .and_then(scheduler::wait_for_task)
            .unwrap_or(SYSCALL_ERROR),
        SYS_CONSOLE_SET => {
            if scheduler::capability_task_id_with_rights(frame[0], capability::CAP_CONSOLE_GRANT)
                .map(scheduler::set_console_owner)
                .unwrap_or(false)
            {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_TASK_RESUME => {
            if scheduler::capability_task_id_with_rights(frame[0], capability::CAP_RESUME)
                .map(scheduler::resume_task)
                .unwrap_or(false)
            {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_WAIT_EVENT => {
            match scheduler::capability_task_id_with_rights(frame[0], capability::CAP_WAIT)
                .and_then(scheduler::wait_for_task_event)
            {
                Some(scheduler::WaitEvent::Exited(status)) => status,
                Some(scheduler::WaitEvent::Stopped) => SYSCALL_STOPPED,
                None => SYSCALL_ERROR,
            }
        }
        SYS_KILL => {
            if scheduler::capability_task_id_with_rights(frame[0], capability::CAP_KILL)
                .map(scheduler::kill_task)
                .unwrap_or(false)
            {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_FS_OPEN => sys_fs_open(frame[0], frame[1]),
        SYS_FS_READ => sys_fs_read(frame[0], frame[1], frame[2]),
        SYS_FS_WRITE => sys_fs_write(frame[0], frame[1], frame[2]),
        SYS_FS_CREATE => sys_fs_create(frame[0]),
        SYS_FS_CLOSE => sys_fs_close(frame[0]),
        SYS_FS_READ_DIR => sys_fs_read_dir(frame[0], frame[1], frame[2]),
        SYS_FS_MKDIR => sys_fs_mkdir(frame[0]),
        SYS_CHDIR => sys_chdir(frame[0]),
        SYS_GETCWD => sys_getcwd(frame[0], frame[1]),
        SYS_FS_UNLINK => sys_fs_unlink(frame[0]),
        SYS_FS_RMDIR => sys_fs_rmdir(frame[0]),
        SYS_FS_RENAME => sys_fs_rename(frame[0], frame[1]),
        SYS_CAP_GET_TASK_ID => scheduler::capability_task_id(frame[0]).unwrap_or(SYSCALL_ERROR),
        SYS_CAP_GRANT => {
            if scheduler::grant_capability(frame[0], frame[1], frame[2] as u32) {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_MAP_PHYS => sys_map_phys(frame[0], frame[1]),
        SYS_BIND_IRQ => {
            if frame[0] <= u64::from(u32::MAX) && scheduler::bind_irq(frame[0] as u32) {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_WAIT_IRQ => {
            if scheduler::wait_irq() {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_STOP_CONSOLE => {
            if scheduler::stop_console_owner() {
                scheduler::schedule();
            }
            0
        }
        SYS_ALLOC_DMA => match scheduler::alloc_dma_for_current(frame[0] as usize) {
            Some((virtual_address, physical_address)) => {
                frame[1] = physical_address as u64;
                virtual_address as u64
            }
            None => SYSCALL_ERROR,
        },
        SYS_PCI_READ => sys_pci_read(frame[0]),
        SYS_PCI_WRITE => sys_pci_write(frame[0], frame[1]),
        _ => SYSCALL_ERROR,
    };

    frame[0] = result;
}

fn sys_map_phys(phys_addr: u64, size: u64) -> u64 {
    scheduler::map_phys_to_current(phys_addr as usize, size as usize)
        .map(|address| address as u64)
        .unwrap_or(SYSCALL_ERROR)
}

fn sys_pci_read(offset: u64) -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_PCI) {
        return SYSCALL_ERROR;
    }
    let Some(address) = pci_config_address(offset) else {
        return SYSCALL_ERROR;
    };
    u64::from(unsafe { core::ptr::read_volatile(address as *const u32) })
}

fn sys_pci_write(offset: u64, value: u64) -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_PCI) {
        return SYSCALL_ERROR;
    }
    let Some(address) = pci_config_address(offset) else {
        return SYSCALL_ERROR;
    };
    unsafe { core::ptr::write_volatile(address as *mut u32, value as u32) };
    0
}

fn pci_config_address(offset: u64) -> Option<usize> {
    if offset % 4 != 0 || offset >= mmu::PCI_ECAM_SIZE as u64 {
        return None;
    }
    Some(memory::phys_to_virt(mmu::PCI_ECAM_BASE + offset as usize))
}

fn sys_spawn_with_args(program: u64, argument: u64, ptr: u64, len: u64) -> u64 {
    if len > ARGUMENT_LIMIT as u64 {
        return SYSCALL_ERROR;
    }
    let mut arguments = vec![0_u8; len as usize];
    if user_memory::copy_from_user(ptr, &mut arguments).is_err() {
        return SYSCALL_ERROR;
    }
    scheduler::spawn_user_with_args_and_cap(program, argument, arguments).unwrap_or(SYSCALL_ERROR)
}

fn sys_arguments(buffer_ptr: u64, capacity: u64) -> u64 {
    let arguments = scheduler::current_task_arguments();
    if arguments.len() > capacity as usize {
        return SYSCALL_ERROR;
    }
    if arguments.is_empty() {
        return 0;
    }
    if user_memory::copy_to_user(&arguments, buffer_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    arguments.len() as u64
}

fn sys_fs_open(path_ptr: u64, flags: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        vfs::open(scheduler::current_task_id(), &cwd, path, flags).unwrap_or(SYSCALL_ERROR)
    })
}

fn sys_fs_create(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        if vfs::create(&cwd, path).is_ok() {
            0
        } else {
            SYSCALL_ERROR
        }
    })
}

fn sys_fs_mkdir(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        if vfs::mkdir(&cwd, path).is_ok() {
            0
        } else {
            SYSCALL_ERROR
        }
    })
}

fn sys_fs_unlink(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        if vfs::unlink(&cwd, path).is_ok() {
            0
        } else {
            SYSCALL_ERROR
        }
    })
}

fn sys_fs_rmdir(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        if vfs::rmdir(&cwd, path, scheduler::cwd_in_use).is_ok() {
            0
        } else {
            SYSCALL_ERROR
        }
    })
}

fn sys_fs_rename(source_ptr: u64, destination_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(source_ptr, |source| {
        with_path(destination_ptr, |destination| {
            if vfs::rename(&cwd, source, destination, scheduler::cwd_in_use).is_ok() {
                0
            } else {
                SYSCALL_ERROR
            }
        })
    })
}

fn sys_chdir(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        let Ok(resolved) = vfs::resolve_directory(&cwd, path) else {
            return SYSCALL_ERROR;
        };
        if scheduler::set_current_task_cwd(resolved) {
            0
        } else {
            SYSCALL_ERROR
        }
    })
}

fn sys_getcwd(buffer_ptr: u64, capacity: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    let bytes = cwd.as_bytes();
    if bytes.len() > capacity as usize {
        return SYSCALL_ERROR;
    }
    if user_memory::copy_to_user(bytes, buffer_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    bytes.len() as u64
}

fn sys_fs_close(fd: u64) -> u64 {
    if vfs::close(scheduler::current_task_id(), fd).is_ok() {
        0
    } else {
        SYSCALL_ERROR
    }
}

fn sys_fs_read(fd: u64, buffer_ptr: u64, len: u64) -> u64 {
    if len > IO_LIMIT as u64 {
        return SYSCALL_ERROR;
    }
    if len == 0 {
        return 0;
    }
    let Ok(bytes) = vfs::read(scheduler::current_task_id(), fd, len as usize) else {
        return SYSCALL_ERROR;
    };
    if !bytes.is_empty() && user_memory::copy_to_user(&bytes, buffer_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    bytes.len() as u64
}

fn sys_fs_write(fd: u64, buffer_ptr: u64, len: u64) -> u64 {
    if len > IO_LIMIT as u64 {
        return SYSCALL_ERROR;
    }
    if len == 0 {
        return 0;
    }
    let mut bytes = vec![0_u8; len as usize];
    if user_memory::copy_from_user(buffer_ptr, &mut bytes).is_err() {
        return SYSCALL_ERROR;
    }
    vfs::write(scheduler::current_task_id(), fd, &bytes)
        .map(|count| count as u64)
        .unwrap_or(SYSCALL_ERROR)
}

fn sys_fs_read_dir(path_ptr: u64, buffer_ptr: u64, capacity: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        let Ok(entries) = vfs::read_dir(&cwd, path) else {
            return SYSCALL_ERROR;
        };
        let count = entries.len().min(capacity as usize).min(64);
        if count == 0 {
            return 0;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(
                entries.as_ptr().cast::<u8>(),
                count * core::mem::size_of::<vfs::DirEntry>(),
            )
        };
        if user_memory::copy_to_user(bytes, buffer_ptr).is_err() {
            return SYSCALL_ERROR;
        }
        count as u64
    })
}

fn with_path(path_ptr: u64, call: impl FnOnce(&str) -> u64) -> u64 {
    let mut bytes = [0_u8; PATH_LIMIT + 1];
    if user_memory::copy_from_user(path_ptr, &mut bytes).is_err() {
        return SYSCALL_ERROR;
    }
    let Some(end) = bytes.iter().position(|byte| *byte == 0) else {
        return SYSCALL_ERROR;
    };
    let Ok(path) = core::str::from_utf8(&bytes[..end]) else {
        return SYSCALL_ERROR;
    };
    call(path)
}

fn sys_task_list(buffer_ptr: u64, capacity: u64) -> u64 {
    let tasks = scheduler::task_infos();
    let count = tasks.len().min(capacity as usize).min(64);
    if count == 0 {
        return 0;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(
            tasks.as_ptr().cast::<u8>(),
            count * core::mem::size_of::<scheduler::TaskInfo>(),
        )
    };
    if user_memory::copy_to_user(bytes, buffer_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    count as u64
}

fn sys_meminfo(info_ptr: u64) -> u64 {
    let total_bytes = memory::DRAM_SIZE as u64;
    let managed_total_bytes = (frame_allocator::total_pages() * memory::PAGE_SIZE) as u64;
    let managed_used_bytes = (frame_allocator::used_pages() * memory::PAGE_SIZE) as u64;
    let free_bytes = (frame_allocator::free_pages() * memory::PAGE_SIZE) as u64;
    let info = MemoryInfo {
        total_bytes,
        used_bytes: total_bytes - managed_total_bytes + managed_used_bytes,
        free_bytes,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const MemoryInfo).cast::<u8>(),
            core::mem::size_of::<MemoryInfo>(),
        )
    };
    if user_memory::copy_to_user(bytes, info_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    0
}

fn sys_print(ptr: u64, len: u64) -> u64 {
    // Background output is deliberately discarded in the interactive MVP.
    // Kernel diagnostics use pl011 directly and remain visible for all tasks.
    if !scheduler::current_task_owns_console() {
        return 0;
    }
    if len == 0 {
        return 0;
    }
    if len > 4096 {
        return SYSCALL_ERROR;
    }
    let mut bytes = vec![0_u8; len as usize];
    if user_memory::copy_from_user(ptr, &mut bytes).is_err() {
        return SYSCALL_ERROR;
    }
    write_console_bytes(&bytes);
    0
}

fn sys_print_hex(value: u64) -> u64 {
    if !scheduler::current_task_owns_console() {
        return 0;
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let digit = ((value >> (60 - index * 4)) & 0xF) as u8;
        *byte = match digit {
            0x0..=0x9 => b'0' + digit,
            _ => b'A' + digit - 0xA,
        };
    }
    write_console_bytes(&bytes);
    0
}

fn sys_print_dec(value: u64) -> u64 {
    if !scheduler::current_task_owns_console() {
        return 0;
    }
    let mut digits = [0_u8; 20];
    let mut start = digits.len();
    let mut rest = value;
    loop {
        start -= 1;
        digits[start] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    write_console_bytes(&digits[start..]);
    0
}

#[cfg(feature = "demo-tasks")]
fn write_console_bytes(bytes: &[u8]) {
    pl011::write_bytes(bytes);
}

#[cfg(not(feature = "demo-tasks"))]
fn write_console_bytes(bytes: &[u8]) {
    const IPC_OUTPUT: u8 = 1;
    const DATA_CAPACITY: usize = ipc::PAYLOAD_SIZE - 2;

    // Console output is drained by whichever task currently owns PL011's
    // interrupt (Howler). Its task ID depends on spawn order, so it must never
    // be hardcoded: another task reusing that ID would swallow the output.
    // Before Howler binds INTID 33 the kernel writes to PL011 directly.
    let Some(sink) = scheduler::console_sink_task() else {
        pl011::write_bytes(bytes);
        return;
    };

    for chunk in bytes.chunks(DATA_CAPACITY) {
        let mut message = ipc::Message {
            sender: scheduler::current_task_id(),
            payload: [0; ipc::PAYLOAD_SIZE],
        };
        message.payload[0] = IPC_OUTPUT;
        message.payload[1] = chunk.len() as u8;
        message.payload[2..2 + chunk.len()].copy_from_slice(chunk);
        match ipc::try_send(sink, message) {
            Ok(()) => {
                // TX IPC and hardware RX share Howler's event wait. Yield so a
                // burst of small print calls cannot fill the bounded mailbox.
                scheduler::notify_bound_task(sink);
                scheduler::yield_current();
            }
            Err(ipc::SendError::TargetDead) => pl011::write_bytes(chunk),
            Err(ipc::SendError::QueueFull) => return,
        }
    }
}

fn sys_send(target: u64, message_ptr: u64) -> u64 {
    sys_send_impl(target, message_ptr, false)
}

fn sys_try_send(target: u64, message_ptr: u64) -> u64 {
    sys_send_impl(target, message_ptr, true)
}

fn sys_send_impl(target: u64, message_ptr: u64, nonblocking: bool) -> u64 {
    let mut message = ipc::Message {
        sender: 0,
        payload: [0; ipc::PAYLOAD_SIZE],
    };
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut message as *mut ipc::Message).cast::<u8>(),
            core::mem::size_of::<ipc::Message>(),
        )
    };
    if user_memory::copy_from_user(message_ptr, bytes).is_err() {
        return SYSCALL_ERROR;
    }
    // Identity is a kernel fact, not user-provided message payload. This also
    // protects nameserver replies and receive_from() from sender spoofing.
    message.sender = scheduler::current_task_id();
    let result = if nonblocking {
        ipc::try_send(target, message)
    } else {
        ipc::send(target, message)
    };
    if result.is_err() {
        return SYSCALL_ERROR;
    }
    0
}

fn sys_receive(buffer_ptr: u64, timeout_ticks: u64) -> u64 {
    let size = core::mem::size_of::<ipc::Message>() as u64;
    // Reject before blocking; copy_to_user checks again after the wakeup.
    if !mmu::valid_user_slice_mut(buffer_ptr, size) {
        return SYSCALL_ERROR;
    }
    let message = match ipc::receive_timeout(timeout_ticks) {
        Ok(message) => message,
        Err(ipc::ReceiveError::Timeout) => return SYSCALL_TIMEOUT,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&message as *const ipc::Message).cast::<u8>(),
            core::mem::size_of::<ipc::Message>(),
        )
    };
    if user_memory::copy_to_user(bytes, buffer_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    0
}

// Nameserver RPC. These syscalls do a full request/reply round trip over
// IPC: build a text request, send it to SCOUT_ID, block in receive()
// for the reply. The nameserver itself never calls these.

fn sys_register_name(name_ptr: u64) -> u64 {
    let Some(name) = read_name(name_ptr) else {
        return SYSCALL_ERROR;
    };
    let Some(request) = build_request(b"R ", &name) else {
        return SYSCALL_ERROR;
    };

    if ipc::send(SCOUT_ID, request).is_err() {
        return SYSCALL_ERROR;
    }
    let reply = ipc::receive_from(SCOUT_ID);
    if reply.text() == &b"OK"[..] {
        0
    } else {
        SYSCALL_ERROR
    }
}

fn sys_unregister_name(name_ptr: u64) -> u64 {
    let Some(name) = read_name(name_ptr) else {
        return SYSCALL_ERROR;
    };
    let Some(request) = build_request(b"U ", &name) else {
        return SYSCALL_ERROR;
    };

    if ipc::send(SCOUT_ID, request).is_err() {
        return SYSCALL_ERROR;
    }
    let reply = ipc::receive_from(SCOUT_ID);
    if reply.text() == &b"OK"[..] {
        0
    } else {
        SYSCALL_ERROR
    }
}

fn sys_lookup_name(name_ptr: u64) -> u64 {
    let Some(name) = read_name(name_ptr) else {
        return 0;
    };
    let Some(request) = build_request(b"L ", &name) else {
        return 0;
    };

    if ipc::send(SCOUT_ID, request).is_err() {
        return SYSCALL_ERROR;
    }
    let reply = ipc::receive_from(SCOUT_ID);
    match parse_id_reply(&reply) {
        Some(id) => id,
        None => 0,
    }
}

/// Copies a bounded NUL-terminated name from task memory.
fn read_name(ptr: u64) -> Option<Vec<u8>> {
    let mut bytes = [0_u8; NAME_LIMIT + 1];
    user_memory::copy_from_user(ptr, &mut bytes).ok()?;
    let end = bytes.iter().position(|byte| *byte == 0)?;
    Some(bytes[..end].to_vec())
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
    if digits.is_empty() {
        return None;
    }
    let mut id = 0_u64;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        id = id.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(id)
}
