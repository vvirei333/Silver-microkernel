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

use alloc::{format, vec, vec::Vec};

use crate::{
    capability, channel, elf, frame_allocator, ipc, memory, mmu, mutex, console, scheduler, timer,
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
/// Maps a non-DRAM physical range as Normal Non-Cacheable user memory.
pub const SYS_MAP_PHYS_NC: u64 = 96;
pub const SYS_BIND_IRQ: u64 = 63;
pub const SYS_WAIT_IRQ: u64 = 64;
pub const SYS_TRY_SEND: u64 = 65;
pub const SYS_STOP_CONSOLE: u64 = 66;
pub const SYS_ALLOC_DMA: u64 = 67;
pub const SYS_PCI_READ: u64 = 68;
pub const SYS_PCI_WRITE: u64 = 69;
pub const SYS_SPAWN_PATH: u64 = 70;
pub const SYS_POWER_OFF: u64 = 71;
/// Reset through PSCI `SYSTEM_RESET`; requires `CAP_POWER`, same as power-off.
pub const SYS_POWER_RESET: u64 = 97;
/// Spawns a dynamically loaded ELF with the AArch64 **Linux** syscall
/// personality (`x8` = Linux syscall number) instead of the Silver table.
pub const SYS_SPAWN_LINUX_PATH: u64 = 98;
pub const SYS_MMAP: u64 = 72;
pub const SYS_MUNMAP: u64 = 73;
pub const SYS_MOUNT: u64 = 80;
pub const SYS_UMOUNT: u64 = 81;
pub const SYS_GETPPID: u64 = 82;
pub const SYS_WAITPID: u64 = 83;
pub const SYS_SPAWN_ARGSV: u64 = 84;
pub const SYS_MEMINFO_V2: u64 = 85;
pub const SYS_HEALTH: u64 = 86;
pub const SYS_FS_REPLACE: u64 = 87;
pub const SYS_FS_USAGE: u64 = 88;

const ARGUMENT_LIMIT: usize = 192;
const PATH_LIMIT: usize = 191;
const IO_LIMIT: usize = 4096;
/// SFS allocation granularity, used for conservative quota forecasts.
const FS_BLOCK_SIZE: usize = 512;
/// Upper bound on a file loaded for execution, so a huge VFS blob cannot be
/// cloned into kernel memory before `elf::load_elf` rejects its segments. This
/// matches the VFS file cap: the heap is only 2 MiB.
const EXEC_FILE_LIMIT: usize = vfs::MAX_FILE_BYTES;
/// A foreign (Linux) binary is larger than the general VFS cap: a static
/// libc `hello` is ~800 KiB. The read is transient and exactly file-sized;
/// `vfs::read_file_limit` still refuses anything past this bound.
const LINUX_EXEC_LIMIT: usize = 1024 * 1024;
/// Upper bound on a versioned spawn-arguments block copied from userspace.
const SPAWN_ARGS_LIMIT: usize = 4096;

/// Well-known Scout task ID.
///
/// * demo boot and the fog PoC start Scout directly after idle, so it is 1;
/// * the normal QEMU boot starts silverd first (id 1) and lets it create
///   Scout second, so Scout is 2.
#[cfg(any(feature = "demo-tasks", feature = "fog"))]
pub const SCOUT_ID: u64 = 1;
#[cfg(all(not(feature = "demo-tasks"), not(feature = "fog")))]
pub const SCOUT_ID: u64 = 2;

/// Longest name the nameserver protocol carries: payload is 32 bytes, two go
/// to the "R "/"L " prefix.
const NAME_LIMIT: usize = ipc::PAYLOAD_SIZE - 2;

/// Written into x0 when the call number is not in the table.
pub const SYSCALL_ERROR: u64 = u64::MAX;
pub const SYSCALL_TIMEOUT: u64 = u64::MAX - 1;
pub const SYSCALL_STOPPED: u64 = u64::MAX - 2;
/// `SYS_WAITPID`: no such child, or its zombie record was already consumed.
pub const SYSCALL_ECHILD: u64 = u64::MAX - 3;

/// Frame layout built by the SVC trampoline: x0-x30, then ELR, then SPSR.
pub(crate) const FRAME_QUADWORDS: usize = 33;

/// Stable userspace ABI for SYS_MEMINFO. All values are byte counts.
#[repr(C)]
struct MemoryInfo {
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
}

#[repr(C)]
struct MemoryInfoV2 {
    version: u64,
    size: u64,
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
    task_frames: u64,
    heap_pages: u64,
    mmap_bytes: u64,
    dma_pages: u64,
    capabilities: u64,
    open_files: u64,
}

#[repr(C)]
struct FsUsageInfo {
    inodes: u64,
    data_blocks: u64,
    metadata_blocks: u64,
    blocks: u64,
}


#[repr(C)]
struct HealthInfo {
    version: u64,
    size: u64,
    total_pages: u64,
    free_pages: u64,
    live_tasks: u64,
    zombies: u64,
    open_files: u64,
    dma_leaked_pages: u64,
    dma_quarantined_pages: u64,
    uptime_ticks: u64,
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
    // Kernel-lock protocol: a task executing kernel code holds the kernel
    // lock. Blocking paths release it around `schedule` and re-acquire on
    // resume, so this guard is dropped exactly once, on the syscall's final
    // return path, right before the trampoline `eret`s back to EL0.
    let _kernel_lock = crate::smp::BklGuard::new();
    let frame = unsafe { &mut *(frame as *mut [u64; FRAME_QUADWORDS]) };
    // P1 diagnostic: frame[32] is the SPSR the trampoline saved at SVC entry
    // (x0-x30 at +0..240, ELR at +248, SPSR at +256 = word 32). Volatile so
    // the store cannot sink past a hang point.
    crate::smp::USER_PSR.store_volatile(frame[32] as u32);
    // P3 diagnostic: coherent timer/GIC delivery snapshot (read-only).
    crate::gic::diag_timer_snapshot();
    let number = frame[8]; // x8 carries the call number
    crate::smp::probe(22);

    // The task's personality is fixed at spawn, so a Linux binary can never
    // reach the Silver table by picking a colliding number.
    let result = if scheduler::current_personality() == scheduler::Personality::Linux {
        crate::linux_abi::dispatch(frame)
    } else {
        match number {
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
                console::read_byte().map(u64::from).unwrap_or(SYSCALL_ERROR)
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_UPTIME => timer::ticks(),
        SYS_MEMINFO => sys_meminfo(frame[0]),
        SYS_MEMINFO_V2 => sys_meminfo_v2(frame[0]),
        SYS_HEALTH => sys_health(frame[0]),
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
        SYS_FS_REPLACE => sys_fs_replace(frame[0], frame[1]),
        SYS_FS_USAGE => sys_fs_usage(frame[0], frame[1]),
        SYS_CAP_GET_TASK_ID => scheduler::capability_task_id(frame[0]).unwrap_or(SYSCALL_ERROR),
        SYS_CAP_GRANT => {
            if scheduler::grant_capability(frame[0], frame[1], frame[2] as u32) {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_MAP_PHYS => sys_map_phys(frame[0], frame[1]),
        SYS_MAP_PHYS_NC => {
            scheduler::map_phys_nc_to_current(frame[0] as usize, frame[1] as usize)
                .map(|address| address as u64)
                .unwrap_or(SYSCALL_ERROR)
        }
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
            if !scheduler::current_task_has_rights(capability::CAP_CONSOLE_GRANT) {
                SYSCALL_ERROR
            } else {
                if scheduler::stop_console_owner() {
                    scheduler::schedule_released();
                }
                0
            }
        }
        SYS_ALLOC_DMA => match scheduler::alloc_dma_for_current(frame[0] as usize) {
            Some((virtual_address, physical_address, _generation)) => {
                frame[1] = physical_address as u64;
                virtual_address as u64
            }
            None => SYSCALL_ERROR,
        },
        SYS_PCI_READ => sys_pci_read(frame[0]),
        SYS_PCI_WRITE => sys_pci_write(frame[0], frame[1]),
        SYS_SPAWN_PATH => sys_spawn_path(frame[0]),
        SYS_SPAWN_LINUX_PATH => sys_spawn_linux_path(frame[0]),
        SYS_POWER_OFF => sys_power_off(),
        SYS_POWER_RESET => sys_power_reset(),
        SYS_MMAP => scheduler::mmap_anonymous(frame[1] as usize, frame[2])
            .map(|address| address as u64)
            .unwrap_or(SYSCALL_ERROR),
        SYS_MUNMAP => {
            if scheduler::munmap(frame[0] as usize, frame[1] as usize) {
                0
            } else {
                SYSCALL_ERROR
            }
        }
        SYS_MOUNT => sys_fs_mount(frame[0], frame[1], frame[2], frame[3]),
        SYS_UMOUNT => sys_fs_umount(frame[0], frame[1]),
        SYS_GETPPID => scheduler::current_task_ppid().unwrap_or(0),
        SYS_WAITPID => sys_waitpid(frame[0], frame[1]),
        SYS_SPAWN_ARGSV => sys_spawn_with_argsv(frame[0], frame[1], frame[2], frame[3]),
        _ => SYSCALL_ERROR,
        }
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

/// `SYS_WAITPID(pid, status_ptr)`: waits for an authorized child (direct child
/// or `CAP_WAIT` holder) and consumes its zombie record. `status_ptr == 0`
/// skips the copyout. A consumed/absent/foreign target returns
/// `SYSCALL_ECHILD`; a bad status pointer returns `SYSCALL_ERROR`.
///
/// Transaction order is peek -> validate destination -> consume -> write.
///
/// The blocking wait releases the kernel lock, but the moment it returns a
/// zombie is only peeked. The destination is then checked for EL0 write access
/// without touching it, so a waiter that loses the race (another task consumes
/// the zombie first) returns ECHILD with an unmodified user buffer. Consume
/// still revalidates pid/state/status before removing the record.
///
/// The check and the final `copy_to_user` run in the same BKL-protected,
/// non-blocking critical section: no `schedule`/BKL release/allocation happens
/// in between, and only the current task can change its own TTBR0 mappings, so
/// the validated pointer stays writable for the write. `copy_to_user` repeats
/// the validation, which is why the post-consume write cannot fault in this
/// memory model.
fn sys_waitpid(pid: u64, status_ptr: u64) -> u64 {
    let Some(status) = scheduler::wait_status_for_pid(pid) else {
        return SYSCALL_ECHILD;
    };
    if status_ptr != 0
        && !mmu::valid_user_slice_mut(status_ptr, core::mem::size_of::<u64>() as u64)
    {
        // Nothing was written and the zombie is untouched: a retry with a
        // valid destination reports the same status exactly once.
        return SYSCALL_ERROR;
    }
    if !scheduler::consume_zombie(pid, status) {
        // Lost the race: another waiter consumed the zombie between the peek
        // and here. The user buffer is still untouched because the copyout
        // happens only after a successful consume.
        return SYSCALL_ECHILD;
    }
    if status_ptr != 0 {
        let bytes = status.to_le_bytes();
        if user_memory::copy_to_user(&bytes[..], status_ptr).is_err() {
            // Unreachable: the same pointer was validated above and no mapping
            // change can occur while the BKL is held. Kept as a defensive
            // error path rather than an unchecked store.
            return SYSCALL_ERROR;
        }
    }
    0
}

/// `SYS_SPAWN_ARGSV(program, argument, block_ptr, block_len)`: validates the
/// versioned argv/envp block and spawns an embedded program with it. The old
/// `SYS_SPAWN_ARGS`/`SYS_ARGS` flat-blob path is untouched.
fn sys_spawn_with_argsv(program: u64, argument: u64, block_ptr: u64, block_len: u64) -> u64 {
    if block_len == 0 || block_len > SPAWN_ARGS_LIMIT as u64 {
        return SYSCALL_ERROR;
    }
    let mut block = vec![0_u8; block_len as usize];
    if user_memory::copy_from_user(block_ptr, &mut block).is_err() {
        return SYSCALL_ERROR;
    }
    let Some(args) = silver_microkernel::parse_spawn_args(&block) else {
        return SYSCALL_ERROR;
    };
    scheduler::spawn_user_with_argv_envp_and_cap(program, argument, args.argv, args.envp)
        .unwrap_or(SYSCALL_ERROR)
}

/// Loads the file at `path` from the VFS and spawns it as a dynamically loaded
/// ELF task. Returns the capability handle to the new task, or `SYSCALL_ERROR`.
///
/// The kernel reads the file itself (rather than the shell shipping the bytes):
/// the ELF is far larger than the 4 KiB single-copy limit, and the loader
/// already validates every byte against the 2 MiB image window.
/// Bundle manifest: `write=<absolute prefix>` (one directive per line, `#`
/// starts a comment). It grants a dynamic ELF a path-scoped write window when
/// it has no global `CAP_FS_WRITE`.
fn bundle_write_scope(cwd: &str, elf_path: &str) -> silver_microkernel::bundle::BundlePolicy {
    const MANIFEST_MAX: usize = 512;
    let none = silver_microkernel::bundle::BundlePolicy::default();
    if elf_path.len() + ".manifest".len() > vfs::MAX_PATH_LEN {
        return none;
    }
    // `notes.elf` is described by `notes.manifest`; anything else gets
    // `.manifest` appended verbatim.
    let manifest = match elf_path.strip_suffix(".elf") {
        Some(stem) => format!("{stem}.manifest"),
        None => format!("{elf_path}.manifest"),
    };
    match vfs::file_len(cwd, &manifest) {
        Ok(length) if length != 0 && length <= MANIFEST_MAX => {}
        _ => return none,
    }
    let Ok(bytes) = vfs::read_file(cwd, &manifest) else {
        return none;
    };
    let Ok(text) = core::str::from_utf8(&bytes) else {
        return none;
    };
    silver_microkernel::bundle::parse_manifest(text)
}

fn sys_spawn_path(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        // Length first: `read_file` would otherwise allocate the whole blob
        // before the limit below could reject it.
        match vfs::file_len(&cwd, path) {
            Ok(length) if length <= EXEC_FILE_LIMIT => {}
            _ => return SYSCALL_ERROR,
        }
        let Ok(bytes) = vfs::read_file(&cwd, path) else {
            return SYSCALL_ERROR;
        };
        if bytes.len() > EXEC_FILE_LIMIT {
            return SYSCALL_ERROR;
        }
        let image = match elf::load_elf(&bytes) {
            Ok(image) => image,
            Err(_) => return SYSCALL_ERROR,
        };
        let policy = bundle_write_scope(&cwd, path);
        // Integrity first, then authenticity. A keyed build requires both;
        // an unkeyed build accepts integrity-only bundles but refuses any
        // manifest that claims a signature it cannot check.
        if silver_microkernel::bundle::verify(
            &bytes,
            &policy,
            silver_microkernel::bundle_sig::trusted_key(),
        )
        .is_err()
        {
            return SYSCALL_ERROR;
        }
        scheduler::spawn_elf_with_policy(image, 0, policy.write_prefix, policy.quota)
            .unwrap_or(SYSCALL_ERROR)
    })
}

/// Loads and runs a foreign (Linux ABI) ELF from the filesystem.
fn sys_spawn_linux_path(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        match vfs::file_len(&cwd, path) {
            Ok(length) if length <= LINUX_EXEC_LIMIT => {}
            _ => return SYSCALL_ERROR,
        }
        let Ok(bytes) = vfs::read_file_limit(&cwd, path, LINUX_EXEC_LIMIT) else {
            return SYSCALL_ERROR;
        };
        if bytes.len() > LINUX_EXEC_LIMIT {
            return SYSCALL_ERROR;
        }
        let Ok(image) = crate::linux_elf::load_linux_elf(&bytes) else {
            return SYSCALL_ERROR;
        };
        // The same bundle policy applies: an unkeyed plain ELF has no digest
        // and passes, while a signed manifest still has to verify.
        let policy = bundle_write_scope(&cwd, path);
        if silver_microkernel::bundle::verify(
            &bytes,
            &policy,
            silver_microkernel::bundle_sig::trusted_key(),
        )
        .is_err()
        {
            return SYSCALL_ERROR;
        }
        let argv = vec![path.as_bytes().to_vec()];
        let envp = vec![
            b"PATH=/bin:/sbin:/usr/bin:/usr/sbin".to_vec(),
            b"HOME=/".to_vec(),
            b"TERM=vt100".to_vec(),
        ];
        scheduler::spawn_linux_elf_with_args(image, policy.write_prefix, policy.quota, argv, envp)
            .unwrap_or(SYSCALL_ERROR)
    })
}

/// Powers the machine down through PSCI `SYSTEM_OFF`. The conduit is selected
/// by the platform in [`crate::power`]: QEMU `virt` needs `virtualization=on`
/// for the HVC conduit, Qualcomm uses the secure monitor. A missing conduit
/// traps and the exception vector reports a panic instead of pretending the
/// machine is off.
fn sys_power_off() -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_POWER) {
        return SYSCALL_ERROR;
    }
    console::write_str("POWER OFF: PSCI SYSTEM_OFF\r\n");
    crate::power::system_off()
}

/// Resets the machine through PSCI `SYSTEM_RESET`; same capability and conduit
/// rules as [`sys_power_off`].
fn sys_power_reset() -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_POWER) {
        return SYSCALL_ERROR;
    }
    console::write_str("POWER OFF: PSCI SYSTEM_RESET\r\n");
    crate::power::system_reset()
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
        // Opening for write is the consent point for later `SYS_FS_WRITE` on
        // this descriptor, so the scope check happens here once.
        if flags & (vfs::OPEN_WRITE | vfs::OPEN_APPEND) != 0
            && !write_path_allowed(&cwd, path)
        {
            return SYSCALL_ERROR;
        }
        vfs::open(scheduler::current_task_id(), &cwd, path, flags).unwrap_or(SYSCALL_ERROR)
    })
}

/// Authorizes a write-side path after resolving it against the task cwd. The
/// manifest prefix must be compared with the same absolute path the backend
/// will operate on; a raw `..` component must never walk out of the scope.
fn write_path_allowed(cwd: &str, path: &str) -> bool {
    match vfs::resolve(cwd, path) {
        Ok(resolved) => scheduler::fs_write_allowed(&resolved),
        Err(_) => false,
    }
}

fn sys_fs_create(path_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        if !write_path_allowed(&cwd, path) {
            return SYSCALL_ERROR;
        }
        // A create over an existing file truncates it: no new inode and no
        // growth. Only a genuinely new name needs the conservative allowance.
        let existing = vfs::file_len(&cwd, path).is_ok();
        let (blocks, inodes) = if existing { (0, 0) } else { (1, 1) };
        if !scheduler::fs_quota_allows(blocks, inodes) {
            return SYSCALL_ERROR;
        }
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
        if !write_path_allowed(&cwd, path) {
            return SYSCALL_ERROR;
        }
        // New directory inode, its first data block and possible parent growth.
        if !scheduler::fs_quota_allows(2, 1) {
            return SYSCALL_ERROR;
        }
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
        if !write_path_allowed(&cwd, path) {
            return SYSCALL_ERROR;
        }
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
        if !write_path_allowed(&cwd, path) {
            return SYSCALL_ERROR;
        }
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
            if !write_path_allowed(&cwd, source)
                || !write_path_allowed(&cwd, destination)
            {
                return SYSCALL_ERROR;
            }
            if vfs::rename(&cwd, source, destination, scheduler::cwd_in_use).is_ok() {
                0
            } else {
                SYSCALL_ERROR
            }
        })
    })
}

fn sys_fs_usage(path_ptr: u64, out_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(path_ptr, |path| {
        let Some(usage) = vfs::usage(&cwd, path) else {
            return SYSCALL_ERROR;
        };
        let info = FsUsageInfo {
            inodes: u64::from(usage.inodes),
            data_blocks: u64::from(usage.data_blocks),
            metadata_blocks: u64::from(usage.metadata_blocks),
            blocks: u64::from(usage.blocks()),
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&info as *const FsUsageInfo).cast::<u8>(),
                core::mem::size_of::<FsUsageInfo>(),
            )
        };
        if user_memory::copy_to_user(bytes, out_ptr).is_err() {
            SYSCALL_ERROR
        } else {
            0
        }
    })
}

fn sys_fs_replace(source_ptr: u64, destination_ptr: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    with_path(source_ptr, |source| {
        with_path(destination_ptr, |destination| {
            if !write_path_allowed(&cwd, source)
                || !write_path_allowed(&cwd, destination)
            {
                return SYSCALL_ERROR;
            }
            if vfs::replace(&cwd, source, destination).is_ok() {
                0
            } else {
                SYSCALL_ERROR
            }
        })
    })
}

/// `SYS_MOUNT(source, target, fstype, flags)`: mounts an in-kernel `ramfs`
/// backend at `target`. The point must already be a directory and must not
/// already carry a mount. The ABI is additive; old call numbers are untouched.
fn sys_fs_mount(source_ptr: u64, target_ptr: u64, fstype_ptr: u64, flags: u64) -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_MOUNT) {
        return SYSCALL_ERROR;
    }
    let cwd = scheduler::current_task_cwd();
    with_path(source_ptr, |source| {
        with_path(target_ptr, |target| {
            with_path(fstype_ptr, |fstype| {
                if vfs::mount(&cwd, target, fstype, source, flags, scheduler::cwd_in_use).is_ok() {
                    0
                } else {
                    SYSCALL_ERROR
                }
            })
        })
    })
}

/// `SYS_UMOUNT(target, flags)`: refuses while the mount has child mounts,
/// open files or a task cwd underneath it.
fn sys_fs_umount(target_ptr: u64, flags: u64) -> u64 {
    if !scheduler::current_task_has_rights(capability::CAP_MOUNT) {
        return SYSCALL_ERROR;
    }
    if flags != 0 {
        return SYSCALL_ERROR;
    }
    let cwd = scheduler::current_task_cwd();
    with_path(target_ptr, |target| {
        if vfs::umount(&cwd, target, scheduler::cwd_in_use).is_ok() {
            0
        } else {
            SYSCALL_ERROR
        }
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
    // Copyout and offset commit are one VFS transaction: an EFAULT leaves the
    // descriptor exactly where it was, so the next read retries the same bytes.
    match vfs::read_with(
        scheduler::current_task_id(),
        fd,
        len as usize,
        |bytes| {
            if bytes.is_empty() {
                Ok(())
            } else {
                user_memory::copy_to_user(bytes, buffer_ptr).map_err(|_| ())
            }
        },
    ) {
        Ok(count) => count as u64,
        Err(()) => SYSCALL_ERROR,
    }
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
    // Conservative growth check: every payload block plus at most two metadata
    // levels. Existing usage is re-read on each call, so the over-estimate is
    // per-operation and cannot accumulate across writes.
    let blocks = (bytes.len().div_ceil(FS_BLOCK_SIZE) + 2) as u32;
    if !scheduler::fs_quota_allows(blocks, 0) {
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

/// Copies a NUL-terminated string byte-by-byte so a valid string ending just
/// before a page boundary does not fail because the full buffer crosses it.
fn copy_cstring(pointer: u64, out: &mut [u8]) -> Option<usize> {
    for (index, slot) in out.iter_mut().enumerate() {
        let address = pointer.checked_add(index as u64)?;
        let mut byte = [0_u8; 1];
        user_memory::copy_from_user(address, &mut byte).ok()?;
        *slot = byte[0];
        if byte[0] == 0 {
            return Some(index);
        }
    }
    None
}

fn with_path(path_ptr: u64, call: impl FnOnce(&str) -> u64) -> u64 {
    let mut bytes = [0_u8; PATH_LIMIT + 1];
    let Some(end) = copy_cstring(path_ptr, &mut bytes) else {
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

fn sys_meminfo_v2(info_ptr: u64) -> u64 {
    let total_bytes = memory::DRAM_SIZE as u64;
    let managed_total_bytes = (frame_allocator::total_pages() * memory::PAGE_SIZE) as u64;
    let managed_used_bytes = (frame_allocator::used_pages() * memory::PAGE_SIZE) as u64;
    let task_id = scheduler::current_task_id();
    let (task_frames, heap_pages, mmap_bytes, dma_pages, capabilities) =
        scheduler::current_resource_counts().unwrap_or((0, 0, 0, 0, 0));
    let info = MemoryInfoV2 {
        version: 2,
        size: core::mem::size_of::<MemoryInfoV2>() as u64,
        total_bytes,
        used_bytes: total_bytes - managed_total_bytes + managed_used_bytes,
        free_bytes: (frame_allocator::free_pages() * memory::PAGE_SIZE) as u64,
        task_frames: task_frames as u64,
        heap_pages: heap_pages as u64,
        mmap_bytes: mmap_bytes as u64,
        dma_pages: dma_pages as u64,
        capabilities: capabilities as u64,
        open_files: vfs::open_count(task_id) as u64,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const MemoryInfoV2).cast::<u8>(),
            core::mem::size_of::<MemoryInfoV2>(),
        )
    };
    if user_memory::copy_to_user(bytes, info_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    0
}

fn sys_health(info_ptr: u64) -> u64 {
    let (live_tasks, zombies) = scheduler::task_counts();
    let info = HealthInfo {
        version: 1,
        size: core::mem::size_of::<HealthInfo>() as u64,
        total_pages: frame_allocator::total_pages() as u64,
        free_pages: frame_allocator::free_pages() as u64,
        live_tasks: live_tasks as u64,
        zombies: zombies as u64,
        open_files: vfs::open_total() as u64,
        dma_leaked_pages: scheduler::dma_leaked_pages() as u64,
        dma_quarantined_pages: scheduler::dma_quarantined_pages() as u64,
        uptime_ticks: timer::ticks(),
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const HealthInfo).cast::<u8>(),
            core::mem::size_of::<HealthInfo>(),
        )
    };
    if user_memory::copy_to_user(bytes, info_ptr).is_err() {
        return SYSCALL_ERROR;
    }
    0
}

fn sys_print(ptr: u64, len: u64) -> u64 {
    // Background output is deliberately discarded in the interactive MVP.
    // Kernel diagnostics use console directly and remain visible for all tasks.
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

/// Writes `len` bytes from a user buffer to the console. Unlike `SYS_PRINT`
/// this has no 4 KiB cap; the buffer is copied in bounded chunks. Used by the
/// Linux ABI's `write(1|2, ...)`.
pub(crate) fn write_user_output(ptr: u64, len: u64) -> u64 {
    if !scheduler::current_task_owns_console() {
        return 0;
    }
    let mut remaining = len as usize;
    let mut address = ptr;
    while remaining != 0 {
        let chunk = remaining.min(4096);
        let mut bytes = vec![0_u8; chunk];
        if user_memory::copy_from_user(address, &mut bytes).is_err() {
            return SYSCALL_ERROR;
        }
        write_console_bytes(&bytes);
        remaining -= chunk;
        address = address.wrapping_add(chunk as u64);
    }
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
    console::write_bytes(bytes);
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
        console::write_bytes(bytes);
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
            Err(ipc::SendError::TargetDead) => console::write_bytes(chunk),
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
        Err(ipc::ReceiveError::TargetDead) => return SYSCALL_ERROR,
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
    let Ok(reply) = ipc::receive_from(SCOUT_ID) else {
        return SYSCALL_ERROR;
    };
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
    let Ok(reply) = ipc::receive_from(SCOUT_ID) else {
        return SYSCALL_ERROR;
    };
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
    let Ok(reply) = ipc::receive_from(SCOUT_ID) else {
        return SYSCALL_ERROR;
    };
    match parse_id_reply(&reply) {
        Some(id) => id,
        None => 0,
    }
}

/// Copies a bounded NUL-terminated name from task memory.
fn read_name(ptr: u64) -> Option<Vec<u8>> {
    let mut bytes = [0_u8; NAME_LIMIT + 1];
    let end = copy_cstring(ptr, &mut bytes)?;
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
