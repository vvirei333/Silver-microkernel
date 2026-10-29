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
/// On-disk superblock: ten little-endian u32 fields (40 bytes).
const SFS_SUPERBLOCK_SIZE: usize = 40;

/// Filesystem server opcodes, shared with the shell's `sfsinfo` command.
const OP_INFO: u8 = 0;
/// Create an empty file if it does not already exist. Payload: `name`.
const OP_TOUCH: u8 = 1;
/// Overwrite a file's contents. Payload: `name` + `data`.
const OP_WRITE_FILE: u8 = 2;
/// Read a file's contents back. Payload: `name`.
const OP_READ_FILE: u8 = 3;

/// Block driver opcodes. `OP_READ_SECTOR` takes the sector number in
/// `payload[1..9]` and replies with a shared DMA window.
const OP_READ_SECTOR: u8 = 1;
const OP_WRITE_SECTOR: u8 = 2;

// --- On-disk layout (must match `format_sfs.py`) --------------------------
const BLOCK_SIZE: usize = 512;
const INODE_SIZE: usize = 64;
/// Direct block pointers per inode; a file may span up to 12 blocks (6 KiB).
const DIRECT_BLOCKS: usize = 12;
/// One single-indirect pointer is reserved for later but not yet followed.
const INODE_INDIRECT_OFFSET: usize = 8 + DIRECT_BLOCKS * 4;
const INODE_KIND_FREE: u32 = 0;
const INODE_KIND_FILE: u32 = 1;
const INODE_KIND_DIR: u32 = 2;
const DIR_ENTRY_SIZE: usize = 32;
const DIR_ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / DIR_ENTRY_SIZE;
const NAME_MAX: usize = DIR_ENTRY_SIZE - 5; // inode(4) + len(1) + name
const MAX_INODES: usize = 128;
/// Bitmap covers 2048 blocks in 256 bytes.
const BITMAP_BYTES: usize = 256;

/// Cached mapping of the block driver's shared DMA window, reused across calls
/// so repeated sector access does not leak the finite user MMIO window.
static mut DMA_WINDOW: Option<(u64, u64)> = None;
/// In-memory free-block bitmap (2048 bits) and the full inode table, loaded
/// once at serve time and flushed back on every mutation.
static mut BITMAP: [u8; BITMAP_BYTES] = [0; BITMAP_BYTES];
static mut INODES: [u8; MAX_INODES * INODE_SIZE] = [0; MAX_INODES * INODE_SIZE];
/// Scratch buffer for reading a file back (direct blocks only, 12 × 512 B).
const READ_BUFFER_SIZE: usize = DIRECT_BLOCKS * BLOCK_SIZE;
static mut READ_BUFFER: [u8; READ_BUFFER_SIZE] = [0; READ_BUFFER_SIZE];

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
    /// Stage codes, reported in the exit status low byte. The previous single
    /// `1` only said "mount failed"; these name the stage the server actually
    /// died in, and the kernel repeats the whole status as `EXIT: ... status=`
    /// even for a task that owns no console.
    const STAGE_NO_DRIVER: u8 = 1;
    const STAGE_SEND_FAILED: u8 = 2;
    const STAGE_NO_REPLY: u8 = 3;
    const STAGE_READ_REJECTED: u8 = 4;
    const STAGE_SHORT_WINDOW: u8 = 5;
    const STAGE_MAP_FAILED: u8 = 6;
    const STAGE_BAD_MAGIC: u8 = 7;

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

    /// Packed diagnostic: the stage code in the low byte, then the attempt
    /// counter, the successful read counter and the last driver status word
    /// (reply code in its low byte, the driver's init failure stage above).
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
    /// Block holding the free-block bitmap (2048 bits in one 512-byte block).
    bitmap_block: u32,
    /// First block of the inode table.
    inode_table_block: u32,
    /// Number of blocks the inode table spans.
    inode_table_blocks: u32,
    /// First data block (everything below this is filesystem metadata).
    data_block_start: u32,
    /// The bytes exactly as they were read from the disk, kept so a rejected
    /// magic can be reported instead of being silently swallowed.
    raw: [u8; SFS_SUPERBLOCK_SIZE],
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(silverd_hint: u64) -> ! {
    syscall::print("SFS SERVER STARTING\r\n");
    // The boot argument is only a hint: this server must not trust it blindly,
    // so it is validated against the task table when announcing readiness.
    let superblock = match mount() {
        Ok(superblock) => superblock,
        Err(reason) => {
            // `print` is discarded for any task that does not own the console,
            // so these lines only reach the UART when the server was handed the
            // console explicitly. They are kept because that is exactly what a
            // debugging run with `demo-tasks` needs.
            syscall::print("SFS: MOUNT FAILED\r\n");
            syscall::print("SFS: stage=");
            syscall::print_dec_u64(u64::from(reason.code));
            syscall::print(" attempts=");
            syscall::print_dec_u64(u64::from(reason.attempts));
            syscall::print(" reads=");
            syscall::print_dec_u64(u64::from(reason.reads));
            syscall::print(" last_status=");
            syscall::print_dec_u64(u64::from(reason.last_status));
            syscall::print("\r\n");
            announce(silverd_hint, Some(reason));
            // Exiting is the only honest answer when the filesystem cannot be
            // served: staying alive would make clients pick this task and fail,
            // hiding the healthy instance that was started earlier.
            //
            // The status carries the whole report instead of a bare `1`: the
            // kernel prints `EXIT: ... status=` for every task, and that is the
            // only channel that survives a task without the console. The low
            // byte is the stage code, so `status & 0xFF` names where the mount
            // gave up (`1..=7`); the upper bytes add the attempt counter, the
            // successful read counter and the last block-driver status word.
            syscall::exit_with_status(reason.summary())
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
    let mut failure = MountFailure::new(MountFailure::STAGE_NO_DRIVER);
    while attempt < MOUNT_ATTEMPTS {
        attempt += 1;

        // The driver is resolved per attempt: it may not exist yet, and a
        // respawned driver has a new ID.
        let Some(driver) = find_task_id(syscall::PROGRAM_BLOCK_DRIVER) else {
            failure = MountFailure::new(MountFailure::STAGE_NO_DRIVER);
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        };

        // Request wire format: payload[0] is the opcode (OP_READ_SECTOR) and
        // payload[1..9] carries the little-endian sector number.
        const SUPERBLOCK_SECTOR: u64 = 0;
        let mut request_payload = [0_u8; 9];
        request_payload[0] = OP_READ_SECTOR;
        request_payload[1..9].copy_from_slice(&SUPERBLOCK_SECTOR.to_le_bytes());
        let request = ipc::Message::new(0, &request_payload);
        syscall::print("SFS: sending request to block driver\r\n");
        if !syscall::send(driver, &request) {
            failure = MountFailure::new(MountFailure::STAGE_SEND_FAILED);
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }

        let mut response = ipc::Message::new(0, &[]);
        syscall::print("SFS: waiting for response from block driver\r\n");
        if !receive_from(driver, &mut response) {
            failure = MountFailure::new(MountFailure::STAGE_NO_REPLY);
            syscall::sleep(MOUNT_POLL_TICKS);
            continue;
        }

        // 1: data is in a shared DMA window, 2: driver not initialized yet,
        // 0: read failed. Only 1 can be inspected. On 2 the next byte names
        // the driver's init failure stage, so both are packed into
        // `last_status`.
        if response.payload[0] != 1 {
            last_status =
                u16::from(response.payload[0]) | (u16::from(response.payload[1]) << 8);
            failure = MountFailure::new(MountFailure::STAGE_READ_REJECTED);
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
            return Err(MountFailure::new(MountFailure::STAGE_SHORT_WINDOW));
        }

        syscall::print("SFS: mapping physical 0x");
        syscall::print_hex_u64(physical_address);
        syscall::print(", len ");
        syscall::print_hex_u64(length);
        syscall::print("\r\n");
        let mapped = syscall::map_phys(physical_address, length);
        // The window is Normal Non-Cacheable memory shared with the device, so
        // it is read byte by byte with volatile loads. A struct-wide
        // `read_volatile` may be lowered to a non-volatile copy.
        let superblock = mapped.map(read_superblock);
        // The driver keeps the DMA window private until the client releases it,
        // so the acknowledgement is sent even when mapping failed.
        acknowledge(driver);

        let Some(superblock) = superblock else {
            return Err(MountFailure::new(MountFailure::STAGE_MAP_FAILED));
        };
        if superblock.magic != SFS_MAGIC {
            // A wrong magic right after a successful read is retried: the very
            // first transfer can complete before the shared window is visible
            // to this task. The observed bytes are kept for the final report.
            let mut leading = [0_u8; 8];
            leading.copy_from_slice(&superblock.raw[..8]);
            failure = MountFailure {
                code: MountFailure::STAGE_BAD_MAGIC,
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

    // Nothing mounted after every attempt: the report names the stage of the
    // last failed attempt, so an unattended log distinguishes "no driver",
    // "driver silent", "read rejected" and "unreadable sector".
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
        bitmap_block: word(24),
        inode_table_block: word(28),
        inode_table_blocks: word(32),
        data_block_start: word(36),
        raw: bytes,
    }
}

/// Reports the mount result to silverd. `hint` comes from the boot argument and
/// is used only when the task table confirms it really is silverd.
///
/// `failure` carries the stage code on an unsuccessful mount: the server never
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
    // Load the free-block bitmap and inode table into memory once; every
    // mutation below flushes the affected blocks back to disk.
    load_storage(&superblock);

    loop {
        let mut request = ipc::Message::new(0, &[]);
        if !syscall::receive(&mut request) {
            continue;
        }

        let reply = match request.payload[0] {
            OP_INFO => info_reply(&superblock),
            OP_TOUCH => match read_name(&request.payload[1..]) {
                Some(name) => match find_task_id(syscall::PROGRAM_BLOCK_DRIVER) {
                    Some(driver) => match sfs_touch(&superblock, driver, name) {
                        Some(inode) => touch_reply(inode),
                        None => ipc::Message::new(0, &[0]),
                    },
                    None => ipc::Message::new(0, &[0]),
                },
                None => ipc::Message::new(0, &[0]),
            },
            OP_WRITE_FILE => match read_name_data(&request.payload[1..]) {
                Some((name, data)) => match find_task_id(syscall::PROGRAM_BLOCK_DRIVER) {
                    Some(driver) => match sfs_write_file(&superblock, driver, name, data) {
                        Ok((inode, size, blocks)) => write_reply(inode, size, &blocks),
                        Err(()) => ipc::Message::new(0, &[0]),
                    },
                    None => ipc::Message::new(0, &[0]),
                },
                None => ipc::Message::new(0, &[0]),
            },
            OP_READ_FILE => match read_name(&request.payload[1..]) {
                Some(name) => match find_task_id(syscall::PROGRAM_BLOCK_DRIVER) {
                    Some(driver) => match sfs_read_file(&superblock, driver, name) {
                        Some(data) => read_reply(data),
                        None => ipc::Message::new(0, &[0]),
                    },
                    None => ipc::Message::new(0, &[0]),
                },
                None => ipc::Message::new(0, &[0]),
            },
            _ => ipc::Message::new(0, &[0]),
        };
        let _ = syscall::send(request.sender, &reply);
    }
}

/// Parses a length-prefixed name from a payload that has already had its opcode
/// byte stripped: `[name_len][name bytes...]`.
fn read_name(payload: &[u8]) -> Option<&str> {
    let name_len = usize::from(*payload.get(0)?);
    if name_len == 0 || name_len > NAME_MAX || 1 + name_len > payload.len() {
        return None;
    }
    core::str::from_utf8(&payload[1..1 + name_len]).ok()
}

/// Parses `[name_len][name][data_len][data]` from an opcode-stripped payload.
fn read_name_data(payload: &[u8]) -> Option<(&str, &[u8])> {
    let name_len = usize::from(*payload.get(0)?);
    if name_len == 0 || name_len > NAME_MAX || 1 + name_len > payload.len() {
        return None;
    }
    let name = core::str::from_utf8(&payload[1..1 + name_len]).ok()?;
    let data_len = usize::from(*payload.get(1 + name_len)?);
    let data_start = 2 + name_len;
    if data_start + data_len > payload.len() {
        return None;
    }
    Some((name, &payload[data_start..data_start + data_len]))
}

fn touch_reply(inode: u32) -> ipc::Message {
    let mut payload = [0_u8; 9];
    payload[0] = 1;
    payload[1..5].copy_from_slice(&inode.to_le_bytes());
    ipc::Message::new(0, &payload)
}

fn write_reply(inode: u32, size: u32, blocks: &[u32]) -> ipc::Message {
    let mut payload = [0_u8; 32];
    payload[0] = 1;
    payload[1..5].copy_from_slice(&inode.to_le_bytes());
    payload[5..9].copy_from_slice(&size.to_le_bytes());
    // Block numbers are allocated contiguously from index 0 and are always
    // >= data_block_start, so a leading zero marks the first unused slot.
    let count = blocks
        .iter()
        .take_while(|&&block| block != 0)
        .count()
        .min(4);
    payload[9] = count as u8;
    for (index, &block) in blocks[..count].iter().enumerate() {
        let offset = 10 + index * 4;
        payload[offset..offset + 4].copy_from_slice(&block.to_le_bytes());
    }
    ipc::Message::new(0, &payload)
}

fn read_reply(data: &[u8]) -> ipc::Message {
    let mut payload = [0_u8; 32];
    payload[0] = 1;
    payload[1..5].copy_from_slice(&(data.len() as u32).to_le_bytes());
    let len = data.len().min(27);
    payload[5..5 + len].copy_from_slice(&data[..len]);
    ipc::Message::new(0, &payload)
}

/// Creates the file if it does not exist; `touch` never truncates an existing
/// file, so a present inode is returned unchanged.
fn sfs_touch(superblock: &SfsSuperblock, driver: u64, name: &str) -> Option<u32> {
    if let Some(inode) = sfs_find_inode(superblock, driver, name) {
        return Some(inode);
    }
    sfs_create_inode(superblock, driver, name)
}

/// Overwrites a file's contents. This is the core of the write path: find (or
/// create) the inode, release its old blocks, allocate fresh ones, write the
/// data through the block driver, and persist the inode and the bitmap.
fn sfs_write_file(
    superblock: &SfsSuperblock,
    driver: u64,
    name: &str,
    data: &[u8],
) -> Result<(u32, u32, [u32; DIRECT_BLOCKS]), ()> {
    let inode = match sfs_find_inode(superblock, driver, name) {
        Some(inode) => inode,
        None => sfs_create_inode(superblock, driver, name).ok_or(())?,
    };

    sfs_release_blocks(inode);
    let needed = data.len().div_ceil(BLOCK_SIZE);
    if needed > DIRECT_BLOCKS {
        return Err(());
    }

    let mut blocks = [0_u32; DIRECT_BLOCKS];
    for slot in blocks.iter_mut().take(needed) {
        *slot = sfs_allocate_block(superblock).ok_or(())?;
    }

    for (index, &block) in blocks.iter().enumerate().take(needed) {
        let start = index * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(data.len());
        let mut sector = [0_u8; BLOCK_SIZE];
        sector[..end - start].copy_from_slice(&data[start..end]);
        if !block_write_sector(driver, u64::from(block), &sector) {
            return Err(());
        }
    }

    inode_put_u32(inode, 4, data.len() as u32);
    for (index, &block) in blocks.iter().enumerate() {
        inode_put_u32(inode, 8 + index * 4, block);
    }
    inode_put_u32(inode, INODE_INDIRECT_OFFSET, 0);
    flush_inode(superblock, driver, inode);
    flush_bitmap(superblock, driver);
    Ok((inode, data.len() as u32, blocks))
}

/// Reads a file's contents into a scratch buffer and returns the populated
/// prefix. Only direct blocks are followed (single-indirect is not yet).
fn sfs_read_file(superblock: &SfsSuperblock, driver: u64, name: &str) -> Option<&'static [u8]> {
    let inode = sfs_find_inode(superblock, driver, name)?;
    let size = inode_get_u32(inode, 4) as usize;
    if size > READ_BUFFER_SIZE {
        return None;
    }
    let mut read = 0_usize;
    for index in 0..DIRECT_BLOCKS {
        let block = inode_get_u32(inode, 8 + index * 4);
        if block == 0 {
            break;
        }
        let mut sector = [0_u8; BLOCK_SIZE];
        if !block_read_sector(driver, u64::from(block), &mut sector) {
            return None;
        }
        let take = (size - read).min(BLOCK_SIZE);
        unsafe { READ_BUFFER[read..read + take].copy_from_slice(&sector[..take]) };
        read += take;
        if read >= size {
            break;
        }
    }
    Some(unsafe { &READ_BUFFER[..read] })
}

/// Allocates one free data block: finds a clear bitmap bit, marks it used and
/// returns the block number. The caller is responsible for flushing the bitmap.
fn sfs_allocate_block(superblock: &SfsSuperblock) -> Option<u32> {
    for block in superblock.data_block_start..superblock.total_blocks {
        if !bitmap_test(block) {
            bitmap_set(block, true);
            return Some(block);
        }
    }
    None
}

/// Creates a new empty file: claims a free inode, records it as a regular file,
/// and adds a directory entry under the root directory.
fn sfs_create_inode(superblock: &SfsSuperblock, driver: u64, name: &str) -> Option<u32> {
    let mut inode = None;
    for index in 1..MAX_INODES as u32 {
        if inode_get_u32(index, 0) == INODE_KIND_FREE {
            inode = Some(index);
            break;
        }
    }
    let inode = inode?;

    inode_put_u32(inode, 0, INODE_KIND_FILE);
    inode_put_u32(inode, 4, 0);
    for index in 0..DIRECT_BLOCKS {
        inode_put_u32(inode, 8 + index * 4, 0);
    }
    inode_put_u32(inode, INODE_INDIRECT_OFFSET, 0);

    if !add_dir_entry(superblock, driver, superblock.root_inode, name, inode) {
        inode_put_u32(inode, 0, INODE_KIND_FREE);
        return None;
    }
    flush_inode(superblock, driver, inode);
    Some(inode)
}

/// Finds the inode named `name` under the root directory, scanning every
/// populated directory block for a matching entry.
fn sfs_find_inode(superblock: &SfsSuperblock, driver: u64, name: &str) -> Option<u32> {
    if name.len() > NAME_MAX {
        return None;
    }
    let mut sector = [0_u8; BLOCK_SIZE];
    for index in 0..DIRECT_BLOCKS {
        let block = inode_get_u32(superblock.root_inode, 8 + index * 4);
        if block == 0 {
            return None;
        }
        if !block_read_sector(driver, u64::from(block), &mut sector) {
            return None;
        }
        for slot in 0..DIR_ENTRIES_PER_BLOCK {
            let offset = slot * DIR_ENTRY_SIZE;
            let entry_inode =
                u32::from_le_bytes(sector[offset..offset + 4].try_into().unwrap());
            if entry_inode == 0 {
                continue;
            }
            let entry_len = usize::from(sector[offset + 4]);
            if entry_len == name.len()
                && &sector[offset + 5..offset + 5 + entry_len] == name.as_bytes()
            {
                return Some(entry_inode);
            }
        }
    }
    None
}

/// Releases every direct block of `inode` back to the free-list.
fn sfs_release_blocks(inode: u32) {
    for index in 0..DIRECT_BLOCKS {
        let block = inode_get_u32(inode, 8 + index * 4);
        if block == 0 {
            continue;
        }
        bitmap_set(block, false);
        inode_put_u32(inode, 8 + index * 4, 0);
    }
}

/// Appends an entry to a directory: reuses a free slot in an existing block, or
/// allocates a new data block for the directory when none is free.
fn add_dir_entry(
    superblock: &SfsSuperblock,
    driver: u64,
    dir_inode: u32,
    name: &str,
    child: u32,
) -> bool {
    if name.len() > NAME_MAX {
        return false;
    }
    let mut entry = [0_u8; DIR_ENTRY_SIZE];
    entry[0..4].copy_from_slice(&child.to_le_bytes());
    entry[4] = name.len() as u8;
    entry[5..5 + name.len()].copy_from_slice(name.as_bytes());

    let mut sector = [0_u8; BLOCK_SIZE];
    for index in 0..DIRECT_BLOCKS {
        let block = inode_get_u32(dir_inode, 8 + index * 4);
        if block == 0 {
            break;
        }
        if !block_read_sector(driver, u64::from(block), &mut sector) {
            return false;
        }
        for slot in 0..DIR_ENTRIES_PER_BLOCK {
            let offset = slot * DIR_ENTRY_SIZE;
            if u32::from_le_bytes(sector[offset..offset + 4].try_into().unwrap()) == 0 {
                sector[offset..offset + DIR_ENTRY_SIZE].copy_from_slice(&entry);
                return block_write_sector(driver, u64::from(block), &sector);
            }
        }
    }

    let Some(block) = sfs_allocate_block(superblock) else {
        return false;
    };
    let mut placed = false;
    for index in 0..DIRECT_BLOCKS {
        if inode_get_u32(dir_inode, 8 + index * 4) == 0 {
            inode_put_u32(dir_inode, 8 + index * 4, block);
            placed = true;
            break;
        }
    }
    if !placed {
        bitmap_set(block, false);
        return false;
    }
    let mut new_sector = [0_u8; BLOCK_SIZE];
    new_sector[..DIR_ENTRY_SIZE].copy_from_slice(&entry);
    if !block_write_sector(driver, u64::from(block), &new_sector) {
        return false;
    }
    flush_inode(superblock, driver, dir_inode);
    flush_bitmap(superblock, driver);
    true
}

/// Loads the free-block bitmap and the whole inode table into memory.
fn load_storage(superblock: &SfsSuperblock) {
    let Some(driver) = find_task_id(syscall::PROGRAM_BLOCK_DRIVER) else {
        return;
    };
    let mut sector = [0_u8; BLOCK_SIZE];
    if block_read_sector(driver, u64::from(superblock.bitmap_block), &mut sector) {
        unsafe {
            for index in 0..BITMAP_BYTES {
                BITMAP[index] = sector[index];
            }
        }
    }
    for index in 0..superblock.inode_table_blocks as usize {
        if block_read_sector(
            driver,
            u64::from(superblock.inode_table_block) + index as u64,
            &mut sector,
        ) {
            let base = index * BLOCK_SIZE;
            unsafe {
                for offset in 0..BLOCK_SIZE {
                    INODES[base + offset] = sector[offset];
                }
            }
        }
    }
}

/// Reads one sector through the block driver and acknowledges the shared window.
fn block_read_sector(driver: u64, sector: u64, data: &mut [u8; BLOCK_SIZE]) -> bool {
    let mut request_payload = [0_u8; 9];
    request_payload[0] = OP_READ_SECTOR;
    request_payload[1..9].copy_from_slice(&sector.to_le_bytes());
    if !syscall::send(driver, &ipc::Message::new(0, &request_payload)) {
        return false;
    }
    let mut response = ipc::Message::new(0, &[]);
    if !receive_from(driver, &mut response) || response.payload[0] != 1 {
        return false;
    }
    let physical = le_u64(&response.payload[1..9]);
    let length = usize::from(u16::from_le_bytes([response.payload[9], response.payload[10]]));
    if length != BLOCK_SIZE {
        acknowledge(driver);
        return false;
    }
    let Some(virtual_address) = window_map(physical, length) else {
        acknowledge(driver);
        return false;
    };
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile((virtual_address as *const u8).add(index)) };
    }
    acknowledge(driver);
    true
}

/// Writes one sector: requests the shared window, fills it, commits, and checks
/// the driver's status reply.
fn block_write_sector(driver: u64, sector: u64, data: &[u8; BLOCK_SIZE]) -> bool {
    let mut request_payload = [0_u8; 9];
    request_payload[0] = OP_WRITE_SECTOR;
    request_payload[1..9].copy_from_slice(&sector.to_le_bytes());
    if !syscall::send(driver, &ipc::Message::new(0, &request_payload)) {
        return false;
    }
    let mut response = ipc::Message::new(0, &[]);
    if !receive_from(driver, &mut response) || response.payload[0] != 1 {
        return false;
    }
    let physical = le_u64(&response.payload[1..9]);
    let length = usize::from(u16::from_le_bytes([response.payload[9], response.payload[10]]));
    let Some(virtual_address) = (length == BLOCK_SIZE).then(|| window_map(physical, length)).flatten()
    else {
        let _ = syscall::send(driver, &ipc::Message::new(0, &[0xA6]));
        return false;
    };
    for (index, byte) in data.iter().enumerate() {
        unsafe { core::ptr::write_volatile((virtual_address as *mut u8).add(index), *byte) };
    }
    if !syscall::send(driver, &ipc::Message::new(0, &[0xA6])) {
        return false;
    }
    let mut status = ipc::Message::new(0, &[]);
    if !receive_from(driver, &mut status) {
        return false;
    }
    status.payload[0] == 1
}

/// Cached mapping of the block driver's shared DMA window.
fn window_map(physical: u64, length: usize) -> Option<u64> {
    unsafe {
        match *core::ptr::addr_of!(DMA_WINDOW) {
            Some((cached_physical, virtual_address)) if cached_physical == physical => {
                Some(virtual_address)
            }
            _ => {
                let virtual_address = syscall::map_phys(physical, length as u64)?;
                core::ptr::addr_of_mut!(DMA_WINDOW).write(Some((physical, virtual_address)));
                Some(virtual_address)
            }
        }
    }
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut buffer = [0_u8; 8];
    buffer.copy_from_slice(bytes);
    u64::from_le_bytes(buffer)
}

fn bitmap_test(block: u32) -> bool {
    let byte = (block / 8) as usize;
    let bit = (block % 8) as usize;
    unsafe { BITMAP[byte] & (1 << bit) != 0 }
}

fn bitmap_set(block: u32, used: bool) {
    let byte = (block / 8) as usize;
    let bit = (block % 8) as usize;
    unsafe {
        if used {
            BITMAP[byte] |= 1 << bit;
        } else {
            BITMAP[byte] &= !(1 << bit);
        }
    }
}

fn inode_get_u32(inode: u32, offset: usize) -> u32 {
    let base = inode as usize * INODE_SIZE + offset;
    u32::from_le_bytes([
        unsafe { INODES[base] },
        unsafe { INODES[base + 1] },
        unsafe { INODES[base + 2] },
        unsafe { INODES[base + 3] },
    ])
}

fn inode_put_u32(inode: u32, offset: usize, value: u32) {
    let base = inode as usize * INODE_SIZE + offset;
    let bytes = value.to_le_bytes();
    unsafe {
        INODES[base] = bytes[0];
        INODES[base + 1] = bytes[1];
        INODES[base + 2] = bytes[2];
        INODES[base + 3] = bytes[3];
    }
}

fn flush_inode(superblock: &SfsSuperblock, driver: u64, inode: u32) {
    let inodes_per_block = superblock.block_size as usize / INODE_SIZE;
    let block_index = inode as usize / inodes_per_block;
    let block = u64::from(superblock.inode_table_block) + block_index as u64;
    let base = block_index * superblock.block_size as usize;
    let mut sector = [0_u8; BLOCK_SIZE];
    for offset in 0..BLOCK_SIZE {
        sector[offset] = unsafe { INODES[base + offset] };
    }
    let _ = block_write_sector(driver, block, &sector);
}

fn flush_bitmap(superblock: &SfsSuperblock, driver: u64) {
    let mut sector = [0_u8; BLOCK_SIZE];
    for offset in 0..BITMAP_BYTES {
        sector[offset] = unsafe { BITMAP[offset] };
    }
    let _ = block_write_sector(driver, u64::from(superblock.bitmap_block), &sector);
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
