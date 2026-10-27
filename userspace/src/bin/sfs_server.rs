#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

/// One timer tick is 10 ms.
const TICKS_PER_SECOND: u64 = 100;
/// The block driver answers as soon as it receives a request, but it may still
/// be probing the PCI device while the filesystem server boots. Retrying keeps
/// the boot order from mattering; giving up keeps a broken driver from wedging
/// the server forever.
const MOUNT_ATTEMPTS: u64 = 20;
const MOUNT_POLL_TICKS: u64 = 50;
/// A sector read is answered immediately. Waiting two seconds keeps the server
/// responsive when the driver died between send and reply.
const RESPONSE_TIMEOUT_TICKS: u64 = 2 * TICKS_PER_SECOND;
const RESPONSE_POLL_TICKS: u64 = 10;

const TASK_LIST_CAPACITY: usize = 32;
const EMPTY_TASK: syscall::TaskInfo = syscall::TaskInfo {
    id: 0,
    state: 0,
    program: 0,
    argument: 0,
};
const TASK_DEAD: u64 = 6;
const TASK_STOPPED: u64 = 7;

const SFS_MAGIC: u32 = 0x5346_5321; // "SFS!"
const SFS_SUPERBLOCK_SIZE: usize = 24;

/// Filesystem server opcodes, shared with the shell's `sfsinfo` command.
const OP_INFO: u8 = 0;

/// Why a mount attempt failed, with enough detail to diagnose an unattended
/// boot: the server never owns the console, so `print` output would be
/// discarded and this struct is the only channel back to silverd.
#[derive(Copy, Clone)]
struct MountFailure {
    code: u8,
    /// First eight bytes of the sector as read back (zero when nothing was read).
    leading: u64,
    /// Physical address of the shared DMA window the driver returned.
    physical: u64,
    /// Temporary probe: first four bytes the block driver itself reads back.
    driver_view: u32,
    /// Temporary probe: the sector number the driver parsed from the request.
    driver_sector: u8,
    attempts: u8,
    reads: u8,
    last_status: u16,
}

impl MountFailure {
    const DRIVER_UNAVAILABLE: u8 = 1;
    const READ_REJECTED: u8 = 2;
    const MAPPING_FAILED: u8 = 3;
    const BAD_FILESYSTEM: u8 = 4;

    /// Fills a report with no diagnostics attached.
    fn new(code: u8) -> Self {
        Self {
            code,
            leading: 0,
            physical: 0,
            driver_view: 0,
            driver_sector: 0,
            attempts: 0,
            reads: 0,
            last_status: 0,
        }
    }

    /// Packed diagnostic: the reason code in the low byte, then the attempt
    /// counter and the last driver status byte.
    fn summary(self) -> u64 {
        u64::from(self.code)
            | u64::from(self.attempts) << 8
            | u64::from(self.reads) << 16
            | u64::from(self.last_status) << 32
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
struct SfsSuperblock {
    magic: u32,
    version: u32,
    block_size: u32,
    total_blocks: u32,
    root_inode: u32,
    inode_count: u32,
    /// The bytes exactly as they were read from the disk, kept so a rejected
    /// magic can be reported instead of being silently swallowed.
    raw: [u8; SFS_SUPERBLOCK_SIZE],
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(silverd_hint: u64) -> ! {
    // The boot argument is only a hint: this server must not trust it blindly,
    // so it is validated against the task table when announcing readiness.
    let superblock = match mount() {
        Ok(superblock) => superblock,
        Err(reason) => {
            announce(silverd_hint, Some(reason));
            // Exiting is the only honest answer when the filesystem cannot be
            // served: staying alive would make clients pick this task and fail,
            // hiding the healthy instance that was started earlier.
            syscall::exit_with_status(1)
        }
    };
    announce(silverd_hint, None);
    serve(superblock)
}

/// Reads sector 0 through the block driver and validates the superblock.
///
/// Returns the mount failure report so the caller can forward it to silverd;
/// the server keeps answering requests only after a successful mount.
fn mount() -> Result<SfsSuperblock, MountFailure> {
    let mut attempt = 0;
    let mut reads = 0_u8;
    let mut last_status = 0xFFFF_u16;
    let mut failure = MountFailure::new(MountFailure::DRIVER_UNAVAILABLE);
    while attempt < MOUNT_ATTEMPTS {
        attempt += 1;

        // The driver is resolved per attempt: it may not exist yet, and a
        // respawned driver has a new ID.
        let Some(driver) = find_task_id(syscall::PROGRAM_BLOCK_DRIVER) else {
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        };

        let request = ipc::Message::new(0, &0_u64.to_le_bytes());
        if !syscall::send(driver, &request) {
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }

        let mut response = ipc::Message::new(0, &[]);
        if !receive_from(driver, &mut response) {
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }

        // 1: data is in a shared DMA window, 2: driver not initialized yet,
        // 0: read failed. Only 1 can be inspected.
        if response.payload[0] != 1 {
            last_status = u16::from(response.payload[0]);
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }
        reads = reads.saturating_add(1);

        let mut physical_bytes = [0_u8; 8];
        let mut length_bytes = [0_u8; 2];
        physical_bytes.copy_from_slice(&response.payload[1..9]);
        length_bytes.copy_from_slice(&response.payload[9..11]);
        let physical_address = u64::from_le_bytes(physical_bytes);
        let length = u64::from(u16::from_le_bytes(length_bytes));

        if length < SFS_SUPERBLOCK_SIZE as u64 {
            acknowledge(driver);
            return Err(MountFailure::new(MountFailure::READ_REJECTED));
        }

        let mapped = syscall::map_phys(physical_address, length);
        // The window is Normal Non-Cacheable memory shared with the device, so
        // it is read byte by byte with volatile loads. A struct-wide
        // `read_volatile` may be lowered to a non-volatile copy.
        let superblock = mapped.map(read_superblock);
        // The driver keeps the DMA window private until the client releases it,
        // so the acknowledgement is sent even when mapping failed.
        acknowledge(driver);

        let Some(superblock) = superblock else {
            return Err(MountFailure::new(MountFailure::MAPPING_FAILED));
        };
        if superblock.magic != SFS_MAGIC {
            // A wrong magic right after a successful read is retried: the very
            // first transfer can complete before the shared window is visible
            // to this task. The observed bytes are kept for the final report.
            let mut leading = [0_u8; 8];
            leading.copy_from_slice(&superblock.raw[..8]);
            failure = MountFailure {
                code: MountFailure::BAD_FILESYSTEM,
                leading: u64::from_le_bytes(leading),
                physical: physical_address,
                driver_view: u32::from_le_bytes(response.payload[11..15].try_into().unwrap()),
                driver_sector: response.payload[15],
                attempts: attempt as u8,
                reads,
                last_status,
            };
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }

        return Ok(superblock);
    }

    // Nothing mounted after every attempt: report the last attempt's state so
    // an unattended log distinguishes "no driver" from "unreadable sector".
    failure.attempts = attempt as u8;
    failure.reads = reads;
    failure.last_status = last_status;
    Err(failure)
}

/// Copies the on-disk superblock out of the driver's DMA window.
///
/// The window is device-owned memory mapped Normal Non-Cacheable, so every byte
/// is fetched with a volatile load; a struct-wide `read_volatile` may be
/// optimised into a non-volatile copy and return stale or torn data.
fn read_superblock(virtual_address: u64) -> SfsSuperblock {
    let base = virtual_address as *const u8;
    let mut bytes = [0_u8; SFS_SUPERBLOCK_SIZE];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile(base.add(index)) };
    }
    // The window is filled by the device, not by this task, so keep the loads
    // above ordered before the magic check: a reordered or hoisted read would
    // observe the zeroed buffer instead of the completed transfer.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let word = |offset: usize| {
        let mut value = [0_u8; 4];
        value.copy_from_slice(&bytes[offset..offset + 4]);
        u32::from_le_bytes(value)
    };
    SfsSuperblock {
        magic: word(0),
        version: word(4),
        block_size: word(8),
        total_blocks: word(12),
        root_inode: word(16),
        inode_count: word(20),
        raw: bytes,
    }
}

/// Reports the mount result to silverd. `hint` comes from the boot argument and
/// is used only when the task table confirms it really is silverd.
///
/// `failure` carries the reason code on an unsuccessful mount: the server never
/// owns the console, so its own `print` output would be discarded and this is
/// the only way the cause reaches an unattended boot log.
fn announce(hint: u64, failure: Option<MountFailure>) {
    let Some(silverd) = find_task_id_preferring(syscall::PROGRAM_SILVERD, hint) else {
        return;
    };
    let message = match failure {
        None => ipc::Message::new(0, b"SFS READY"),
        // Text stays NUL-terminated so silverd can compare it as a string. Three
        // diagnostic words follow the terminator: the packed summary, the bytes
        // read back from the sector, and the DMA window physical address.
        Some(failure) => {
            let mut payload = [0_u8; 32];
            payload[..9].copy_from_slice(b"SFS ERROR");
            payload[10..18].copy_from_slice(&failure.summary().to_le_bytes());
            payload[18..26].copy_from_slice(&failure.leading.to_le_bytes());
            payload[26..30].copy_from_slice(&failure.driver_view.to_le_bytes());
            payload[30] = failure.driver_sector;
            ipc::Message::new(0, &payload)
        }
    };
    let _ = syscall::send(silverd, &message);
}

/// Answers filesystem queries. Any live client may ask; the reply always goes
/// to `request.sender`, which the kernel fills in, so no client is hardcoded.
fn serve(superblock: SfsSuperblock) -> ! {
    loop {
        let mut request = ipc::Message::new(0, &[]);
        if !syscall::receive(&mut request) {
            continue;
        }

        let reply = if request.payload[0] == OP_INFO {
            info_reply(&superblock)
        } else {
            ipc::Message::new(0, &[0])
        };
        let _ = syscall::send(request.sender, &reply);
    }
}

fn info_reply(superblock: &SfsSuperblock) -> ipc::Message {
    let mut payload = [0_u8; 21];
    payload[0] = 1;
    payload[1..5].copy_from_slice(&superblock.block_size.to_le_bytes());
    payload[5..9].copy_from_slice(&superblock.total_blocks.to_le_bytes());
    payload[9..13].copy_from_slice(&superblock.root_inode.to_le_bytes());
    payload[13..17].copy_from_slice(&superblock.inode_count.to_le_bytes());
    payload[17..21].copy_from_slice(&superblock.magic.to_le_bytes());
    ipc::Message::new(0, &payload)
}

/// Releases the driver's shared DMA window. Best effort: a driver that already
/// exited reclaims its buffer with the task.
fn acknowledge(driver: u64) {
    let acknowledgement = ipc::Message::new(0, &[0xA5]);
    let _ = syscall::send(driver, &acknowledgement);
}

/// Waits for a reply from `task_id` only, bounded so a driver that dies without
/// answering cannot block the server. Unrelated queued messages are discarded:
/// nothing else talks to the filesystem server.
fn receive_from(task_id: u64, buffer: &mut ipc::Message) -> bool {
    let deadline = syscall::uptime_ticks().saturating_add(RESPONSE_TIMEOUT_TICKS);
    loop {
        if !syscall::task_alive(task_id) {
            return false;
        }
        match syscall::receive_timeout(buffer, RESPONSE_POLL_TICKS) {
            Ok(()) => {
                if buffer.sender == task_id {
                    return true;
                }
            }
            Err(_) => {
                if syscall::uptime_ticks() >= deadline {
                    return false;
                }
            }
        }
    }
}

/// Resolves the live task whose executable is `program`.
///
/// Task IDs depend on the spawn order of the whole boot, so nothing may assume
/// them. Dead and stopped tasks are skipped, and the most recently spawned match
/// wins so a restarted driver is preferred over its stale predecessor.
fn find_task_id(program: u64) -> Option<u64> {
    find_task_id_preferring(program, 0)
}

fn find_task_id_preferring(program: u64, preferred: u64) -> Option<u64> {
    let mut tasks = [EMPTY_TASK; TASK_LIST_CAPACITY];
    let count = syscall::task_list(&mut tasks)?;
    let tasks = &tasks[..count.min(TASK_LIST_CAPACITY)];
    let live = |task: &syscall::TaskInfo| {
        task.program == program && task.state != TASK_DEAD && task.state != TASK_STOPPED
    };
    tasks
        .iter()
        .rev()
        .find(|task| task.id == preferred && live(task))
        .or_else(|| tasks.iter().rev().find(|task| live(task)))
        .map(|task| task.id)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
