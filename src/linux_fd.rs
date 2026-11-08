//! Linux process file-descriptor table.
//!
//! The Silver VFS is the storage layer, but Linux file descriptors are process
//! state with semantics the VFS does not model: fd 0/1/2 are console handles,
//! `/dev/*` are synthetic devices, and `dup2` can point stdout at a file.
//! Everything Linux-specific lives here, behind the `linux-abi` feature; the
//! kernel and the VFS never depend on it.
//!
//! A table maps small Linux fd numbers to one of:
//! - `Console`: the process's stdin/stdout/stderr (the initial 0,1,2);
//! - a synthetic device (`/dev/null`, `/dev/zero`, `/dev/full`,
//!   `/dev/urandom`), which needs no VFS inode;
//! - a real VFS descriptor (3+), created through `vfs::open`/`vfs::open_dir`.
//!
//! The table is per task and is dropped when the task exits; the VFS's own
//! `close_task` releases the underlying handles.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::{mutex::Mutex, scheduler, user_memory, vfs};

/// Kernel pipe buffer. Linux guarantees atomic writes up to `PIPE_BUF`; 4 KiB
/// is the usual capacity and is enough for the console OS.
const PIPE_CAPACITY: usize = 4096;

/// Shared state of one pipe. Both descriptors and every fork/dup refer to the
/// same `Arc`, so data written by one end is seen by the other.
pub struct PipeState {
    buffer: VecDeque<u8>,
    read_ends: usize,
    write_ends: usize,
}

/// Shared state of one `memfd_create` file. The offset lives in the shared
/// object, so `dup` shares it exactly like a real file description.
pub struct MemFdState {
    data: Vec<u8>,
    offset: usize,
}

/// Linux fd targets. `Clone` (not `Copy`) because a pipe/eventfd/memfd target
/// shares an `Arc`; cloning is what `dup`/`fork` do deliberately.
#[derive(Clone)]
enum Target {
    Console,
    Null,
    Zero,
    Full,
    Random,
    Vfs(u64),
    Pipe {
        state: Arc<Mutex<PipeState>>,
        read: bool,
    },
    EventFd {
        state: Arc<Mutex<u64>>,
        nonblock: bool,
    },
    MemFd(Arc<Mutex<MemFdState>>),
}

struct Table {
    task: u64,
    slots: Vec<Option<Target>>,
}

static TABLES: Mutex<Vec<Table>> = Mutex::new(Vec::new());

const MAX_FDS: usize = 64;

fn with_table<R>(task: u64, action: impl FnOnce(&mut Table) -> R) -> R {
    let mut tables = TABLES.lock();
    let index = match tables.iter().position(|table| table.task == task) {
        Some(index) => index,
        None => {
            let mut slots = Vec::with_capacity(8);
            slots.push(Some(Target::Console));
            slots.push(Some(Target::Console));
            slots.push(Some(Target::Console));
            tables.push(Table { task, slots });
            tables.len() - 1
        }
    };
    action(&mut tables[index])
}

fn lookup(task: u64, fd: u64) -> Option<Target> {
    with_table(task, |table| {
        table.slots.get(fd as usize).cloned().flatten()
    })
}

impl Target {
    /// A new descriptor referring to the same pipe end.
    fn retain_endpoint(&self) {
        if let Target::Pipe { state, read } = self {
            let mut pipe = state.lock();
            if *read {
                pipe.read_ends += 1;
            } else {
                pipe.write_ends += 1;
            }
        }
    }

    /// One descriptor of this pipe end went away.
    fn release_endpoint(&self) {
        if let Target::Pipe { state, read } = self {
            let mut pipe = state.lock();
            if *read {
                pipe.read_ends = pipe.read_ends.saturating_sub(1);
            } else {
                pipe.write_ends = pipe.write_ends.saturating_sub(1);
            }
        }
    }
}

/// Installs `target` at the lowest free slot, growing the table.
fn insert(task: u64, target: Target) -> Option<u64> {
    with_table(task, |table| {
        for (index, slot) in table.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(target);
                return Some(index as u64);
            }
        }
        if table.slots.len() >= MAX_FDS {
            return None;
        }
        table.slots.push(Some(target));
        Some((table.slots.len() - 1) as u64)
    })
}

/// Installs `target` at or above `minimum` (for `fcntl(F_DUPFD)`).
fn insert_from(task: u64, target: Target, minimum: u64) -> Option<u64> {
    with_table(task, |table| {
        let mut index = minimum as usize;
        while index < table.slots.len() {
            if table.slots[index].is_none() {
                table.slots[index] = Some(target);
                return Some(index as u64);
            }
            index += 1;
        }
        while table.slots.len() < index {
            if table.slots.len() >= MAX_FDS {
                return None;
            }
            table.slots.push(None);
        }
        if table.slots.len() >= MAX_FDS {
            return None;
        }
        table.slots.push(Some(target));
        Some((table.slots.len() - 1) as u64)
    })
}

/// `openat(2)`. Write flags are rejected for real files (the VFS view is
/// read-only), but synthetic devices accept writes.
pub fn open(task: u64, cwd: &str, path: &str, flags: u64) -> Result<u64, u64> {
    const O_WRONLY: u64 = 0o1;
    const O_RDWR: u64 = 0o2;
    const O_CREAT: u64 = 0o100;
    const O_TRUNC: u64 = 0o1000;
    const O_APPEND: u64 = 0o2000;
    const O_DIRECTORY: u64 = 0o200000;

    let resolved = vfs::resolve(cwd, path).map_err(|_| ENOENT)?;
    let device = match resolved.as_str() {
        "/dev/null" => Some(Target::Null),
        "/dev/zero" => Some(Target::Zero),
        "/dev/full" => Some(Target::Full),
        "/dev/urandom" | "/dev/random" => Some(Target::Random),
        _ => None,
    };
    if let Some(device) = device {
        return insert(task, device).ok_or(EMFILE);
    }
    if flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND) != 0 {
        return Err(EROFS);
    }
    if flags & O_DIRECTORY != 0 {
        let fd = vfs::open_dir(task, cwd, path).map_err(|_| ENOENT)?;
        return insert(task, Target::Vfs(fd)).ok_or(EMFILE);
    }
    // Linux allows opening a directory without O_DIRECTORY.
    let vfs_fd = match vfs::open(task, cwd, path, 0) {
        Ok(fd) => fd,
        Err(_) => vfs::open_dir(task, cwd, path).map_err(|_| ENOENT)?,
    };
    match insert(task, Target::Vfs(vfs_fd)) {
        Some(fd) => Ok(fd),
        None => {
            let _ = vfs::close(task, vfs_fd);
            Err(EMFILE)
        }
    }
}

pub fn close(task: u64, fd: u64) -> Result<(), u64> {
    let target = with_table(task, |table| {
        let slot = table.slots.get_mut(fd as usize)?;
        slot.take()
    })
    .ok_or(EBADF)?;
    target.release_endpoint();
    if let Target::Vfs(vfs_fd) = target {
        let _ = vfs::close(task, vfs_fd);
    }
    Ok(())
}

pub fn dup(task: u64, fd: u64) -> Result<u64, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let new_fd = insert(task, target.clone()).ok_or(EMFILE)?;
    target.retain_endpoint();
    Ok(new_fd)
}

pub fn dup_from(task: u64, fd: u64, minimum: u64) -> Result<u64, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let new_fd = insert_from(task, target.clone(), minimum).ok_or(EMFILE)?;
    target.retain_endpoint();
    Ok(new_fd)
}

pub fn dup2(task: u64, oldfd: u64, newfd: u64) -> Result<u64, u64> {
    if oldfd == newfd {
        return if lookup(task, oldfd).is_some() {
            Ok(newfd)
        } else {
            Err(EBADF)
        };
    }
    let target = lookup(task, oldfd).ok_or(EBADF)?;
    if newfd as usize >= MAX_FDS {
        return Err(EBADF);
    }
    let displaced = with_table(task, |table| {
        while table.slots.len() <= newfd as usize {
            table.slots.push(None);
        }
        table.slots[newfd as usize].take()
    });
    if let Some(displaced) = displaced {
        displaced.release_endpoint();
        if let Target::Vfs(vfs_fd) = displaced {
            let _ = vfs::close(task, vfs_fd);
        }
    }
    target.retain_endpoint();
    with_table(task, |table| table.slots[newfd as usize] = Some(target));
    Ok(newfd)
}

pub fn read(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    if count == 0 {
        return Ok(0);
    }
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        // stdin is not wired to the console yet; report EOF so a reader does
        // not spin.
        Target::Console => Ok(0),
        Target::Null | Target::Full => Ok(0),
        Target::Zero => {
            let chunk = alloc::vec![0_u8; (count as usize).min(64 * 1024)];
            user_memory::copy_to_user(&chunk, buffer).map_err(|_| EFAULT)?;
            Ok(chunk.len() as u64)
        }
        Target::Random => {
            let length = (count as usize).min(64 * 1024);
            let mut bytes = alloc::vec![0_u8; length];
            let mut state = crate::timer::ticks() ^ 0x9E37_79B9_7F4A_7C15;
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            user_memory::copy_to_user(&bytes, buffer).map_err(|_| EFAULT)?;
            Ok(length as u64)
        }
        Target::Vfs(vfs_fd) => {
            if vfs::fd_kind(task, vfs_fd) == Some(vfs::FD_KIND_DIR) {
                return Err(EISDIR);
            }
            vfs::read_with(task, vfs_fd, (count as usize).min(1 << 20), |bytes| {
                if bytes.is_empty() {
                    Ok(())
                } else {
                    user_memory::copy_to_user(bytes, buffer).map_err(|_| ())
                }
            })
            .map(|read| read as u64)
            .map_err(|_| EBADF)
        }
        Target::Pipe { state, read } => {
            if !read {
                return Err(EBADF);
            }
            pipe_read(&state, buffer, count)
        }
        Target::EventFd { state, nonblock } => eventfd_read(&state, nonblock, buffer),
        Target::MemFd(state) => memfd_read(&state, buffer, count),
    }
}

pub fn write(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        Target::Console => {
            if crate::syscall::write_user_output_shared(buffer, count)
                == crate::syscall::SYSCALL_ERROR
            {
                Err(EFAULT)
            } else {
                Ok(count)
            }
        }
        Target::Null | Target::Zero | Target::Random => Ok(count),
        Target::Full => Err(ENOSPC),
        Target::Vfs(vfs_fd) => {
            let mut bytes = alloc::vec![0_u8; (count as usize).min(1 << 20)];
            user_memory::copy_from_user(buffer, &mut bytes).map_err(|_| EFAULT)?;
            vfs::write(task, vfs_fd, &bytes)
                .map(|written| written as u64)
                .map_err(|_| EBADF)
        }
        Target::Pipe { state, read } => {
            if read {
                return Err(EBADF);
            }
            pipe_write(&state, buffer, count)
        }
        Target::EventFd { state, .. } => eventfd_write(&state, buffer),
        Target::MemFd(state) => memfd_write(&state, buffer, count),
    }
}

pub fn lseek(task: u64, fd: u64, offset: i64, whence: u64) -> Result<u64, u64> {
    match lookup(task, fd).ok_or(EBADF)? {
        Target::Vfs(vfs_fd) => vfs::lseek(task, vfs_fd, offset, whence).map_err(|_| EINVAL),
        Target::MemFd(state) => {
            let mut file = state.lock();
            let base = match whence {
                0 => 0,
                1 => file.offset as i64,
                2 => file.data.len() as i64,
                _ => return Err(EINVAL),
            };
            let position = base.checked_add(offset).ok_or(EINVAL)?;
            if position < 0 {
                return Err(EINVAL);
            }
            file.offset = position as usize;
            Ok(position as u64)
        }
        _ => Err(ESPIPE),
    }
}

/// Fills the aarch64 `struct stat` for a descriptor.
pub fn fstat(task: u64, fd: u64, buffer: u64) -> Result<(), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        Target::Console => write_stat(buffer, S_IFCHR | 0o620, 0, fd.max(1)),
        Target::Null | Target::Zero | Target::Full | Target::Random => {
            write_stat(buffer, S_IFCHR | 0o666, 0, fd.max(1))
        }
        Target::Vfs(vfs_fd) => {
            let size = vfs::fd_len(task, vfs_fd).map_err(|_| EBADF)?;
            let kind = vfs::fd_kind(task, vfs_fd).unwrap_or(vfs::FD_KIND_FILE);
            let mode = if kind == vfs::FD_KIND_DIR {
                S_IFDIR | 0o755
            } else {
                S_IFREG | 0o644
            };
            write_stat(buffer, mode, size as u64, fd)
        }
        Target::Pipe { .. } => write_stat(buffer, S_IFIFO | 0o600, 0, fd.max(1)),
        Target::EventFd { .. } => write_stat(buffer, S_IFREG | 0o600, 0, fd.max(1)),
        Target::MemFd(state) => {
            let size = state.lock().data.len() as u64;
            write_stat(buffer, S_IFREG | 0o600, size, fd.max(1))
        }
    }
}

/// Path of an open directory descriptor, for `*at` calls.
pub fn directory_path(task: u64, fd: u64) -> Option<String> {
    match lookup(task, fd)? {
        Target::Vfs(vfs_fd) if vfs::fd_kind(task, vfs_fd) == Some(vfs::FD_KIND_DIR) => {
            vfs::fd_path(task, vfs_fd)
        }
        _ => None,
    }
}

pub fn getdents64(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    const HEADER: usize = 19; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)
    let Target::Vfs(vfs_fd) = lookup(task, fd).ok_or(EBADF)? else {
        return Err(ENOTDIR);
    };
    if count == 0 {
        return Ok(0);
    }
    let (entries, start) = vfs::dir_entries_from_fd(task, vfs_fd, 128).map_err(|_| ENOTDIR)?;
    if entries.is_empty() {
        return Ok(0);
    }
    let limit = (count as usize).min(64 * 1024);
    let mut output = alloc::vec![0_u8; limit];
    let mut used = 0usize;
    let mut serialized = 0usize;
    for entry in &entries {
        let name_len = (entry.name_len as usize).min(entry.name.len());
        let record = (HEADER + name_len + 1 + 7) & !7;
        if used + record > limit {
            break;
        }
        output[used..used + 8].copy_from_slice(&((start + serialized + 1) as u64).to_le_bytes());
        output[used + 8..used + 16]
            .copy_from_slice(&((start + serialized + 1) as i64).to_le_bytes());
        output[used + 16..used + 18].copy_from_slice(&(record as u16).to_le_bytes());
        output[used + 18] = if entry.kind == vfs::FD_KIND_DIR { 4 } else { 8 };
        output[used + HEADER..used + HEADER + name_len].copy_from_slice(&entry.name[..name_len]);
        used += record;
        serialized += 1;
    }
    if serialized == 0 {
        return Err(EINVAL);
    }
    user_memory::copy_to_user(&output[..used], buffer).map_err(|_| EFAULT)?;
    vfs::advance_dir_fd(task, vfs_fd, serialized).map_err(|_| EBADF)?;
    Ok(used as u64)
}

/// Creates a pipe and installs both ends into `task`'s table.
pub fn pipe(task: u64) -> Result<(u64, u64), u64> {    let state = Arc::new(Mutex::new(PipeState {
        buffer: VecDeque::new(),
        read_ends: 1,
        write_ends: 1,
    }));
    let read_fd = insert(
        task,
        Target::Pipe {
            state: state.clone(),
            read: true,
        },
    )
    .ok_or(EMFILE)?;
    match insert(
        task,
        Target::Pipe {
            state,
            read: false,
        },
    ) {
        Some(write_fd) => Ok((read_fd, write_fd)),
        None => {
            let _ = close(task, read_fd);
            Err(EMFILE)
        }
    }
}

/// Creates an eventfd with an initial counter.
pub fn eventfd(task: u64, initial: u64, nonblock: bool) -> Result<u64, u64> {
    insert(
        task,
        Target::EventFd {
            state: Arc::new(Mutex::new(initial)),
            nonblock,
        },
    )
    .ok_or(EMFILE)
}

/// Creates an anonymous in-memory file (`memfd_create`). File-backed `mmap`
/// is not implemented yet, so this is useful for read/write scratch storage.
pub fn memfd(task: u64) -> Result<u64, u64> {
    insert(
        task,
        Target::MemFd(Arc::new(Mutex::new(MemFdState {
            data: Vec::new(),
            offset: 0,
        }))),
    )
    .ok_or(EMFILE)
}

fn eventfd_read(
    state: &Arc<Mutex<u64>>,
    nonblock: bool,
    buffer: u64,
) -> Result<u64, u64> {
    loop {
        let value = {
            let mut counter = state.lock();
            if *counter == 0 {
                None
            } else {
                let value = *counter;
                *counter = 0;
                Some(value)
            }
        };
        if let Some(value) = value {
            user_memory::copy_to_user(&value.to_le_bytes(), buffer).map_err(|_| EFAULT)?;
            return Ok(8);
        }
        if nonblock {
            return Err(EAGAIN);
        }
        scheduler::sleep(1);
    }
}

fn eventfd_write(state: &Arc<Mutex<u64>>, buffer: u64) -> Result<u64, u64> {
    let mut raw = [0_u8; 8];
    user_memory::copy_from_user(buffer, &mut raw).map_err(|_| EFAULT)?;
    let add = u64::from_le_bytes(raw);
    let mut counter = state.lock();
    if u64::MAX - *counter < add {
        return Err(EAGAIN);
    }
    *counter += add;
    Ok(8)
}

fn memfd_read(
    state: &Arc<Mutex<MemFdState>>,
    buffer: u64,
    count: u64,
) -> Result<u64, u64> {
    let mut file = state.lock();
    let available = file.data.len().saturating_sub(file.offset);
    let take = (count as usize).min(available);
    if take == 0 {
        return Ok(0);
    }
    let chunk = file.data[file.offset..file.offset + take].to_vec();
    file.offset += take;
    drop(file);
    user_memory::copy_to_user(&chunk, buffer).map_err(|_| EFAULT)?;
    Ok(take as u64)
}

fn memfd_write(
    state: &Arc<Mutex<MemFdState>>,
    buffer: u64,
    count: u64,
) -> Result<u64, u64> {
    let total = (count as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; total];
    user_memory::copy_from_user(buffer, &mut bytes).map_err(|_| EFAULT)?;
    let mut file = state.lock();
    let start = file.offset;
    let end = start.checked_add(total).ok_or(EINVAL)?;
    if end > file.data.len() {
        file.data.resize(end, 0);
    }
    file.data[start..end].copy_from_slice(&bytes);
    file.offset = end;
    Ok(total as u64)
}

/// Blocking `read` on a pipe. EOF only when every write end is closed.
fn pipe_read(state: &Arc<Mutex<PipeState>>, buffer: u64, count: u64) -> Result<u64, u64> {
    let request = (count as usize).min(PIPE_CAPACITY);
    loop {
        let mut chunk = Vec::new();
        {
            let mut pipe = state.lock();
            if !pipe.buffer.is_empty() {
                let take = request.min(pipe.buffer.len());
                chunk.extend(pipe.buffer.drain(..take));
            } else if pipe.write_ends == 0 {
                return Ok(0);
            }
        }
        if !chunk.is_empty() {
            user_memory::copy_to_user(&chunk, buffer).map_err(|_| EFAULT)?;
            return Ok(chunk.len() as u64);
        }
        // Wait for the writer. Sleeping releases the kernel lock, so the other
        // end can run; the loop re-checks the buffer.
        scheduler::sleep(1);
    }
}

fn pipe_write(state: &Arc<Mutex<PipeState>>, buffer: u64, count: u64) -> Result<u64, u64> {
    let total = (count as usize).min(PIPE_CAPACITY);
    let mut bytes = alloc::vec![0_u8; total];
    user_memory::copy_from_user(buffer, &mut bytes).map_err(|_| EFAULT)?;
    let mut written = 0;
    while written < total {
        let progress: usize;
        {
            let mut pipe = state.lock();
            if pipe.read_ends == 0 {
                return Err(EPIPE);
            }
            let space = PIPE_CAPACITY - pipe.buffer.len();
            progress = space.min(total - written);
            for &byte in &bytes[written..written + progress] {
                pipe.buffer.push_back(byte);
            }
        }
        written += progress;
        if written < total {
            scheduler::sleep(1);
        }
    }
    Ok(total as u64)
}

/// Readiness bits for `ppoll`. `None` means the descriptor is not open.
pub fn poll_ready(task: u64, fd: u64) -> Option<i16> {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    let target = lookup(task, fd)?;
    Some(match target {
        Target::Console | Target::Null => POLLIN | POLLOUT,
        Target::Zero | Target::Random => POLLIN,
        Target::Full => POLLOUT | POLLERR,
        Target::Vfs(_) => POLLIN,
        Target::Pipe { state, read } => {
            let pipe = state.lock();
            if read {
                let mut ready = 0;
                if !pipe.buffer.is_empty() {
                    ready |= POLLIN;
                }
                if pipe.write_ends == 0 {
                    ready |= POLLHUP;
                }
                ready
            } else {
                let mut ready = 0;
                if pipe.buffer.len() < PIPE_CAPACITY {
                    ready |= POLLOUT;
                }
                if pipe.read_ends == 0 {
                    ready |= POLLERR;
                }
                ready
            }
        }
        Target::EventFd { state, .. } => {
            let mut ready = POLLOUT;
            if *state.lock() != 0 {
                ready |= POLLIN;
            }
            ready
        }
        Target::MemFd(_) => POLLIN | POLLOUT,
    })
}

/// Drops the table when a task exits. The VFS handles are released by
/// `vfs::close_task`, which runs in the same teardown path.
pub fn close_task(task: u64) {
    let mut tables = TABLES.lock();
    if let Some(index) = tables.iter().position(|table| table.task == task) {
        let table = tables.remove(index);
        for slot in table.slots.iter().flatten() {
            slot.release_endpoint();
        }
    }
}

/// Copies the parent's descriptor table to a forked child. VFS handle numbers
/// were duplicated with the same numbers by `vfs::dup_owner`, so the targets
/// stay valid. Pipe targets share their `Arc` and gain one endpoint each.
pub fn fork_table(parent: u64, child: u64) {
    let mut tables = TABLES.lock();
    let Some(source) = tables.iter().find(|table| table.task == parent) else {
        return;
    };
    let slots = source.slots.clone();
    for slot in slots.iter().flatten() {
        slot.retain_endpoint();
    }
    tables.retain(|table| table.task != child);
    tables.push(Table { task: child, slots });
}

pub(crate) fn write_stat(buffer: u64, mode: u32, size: u64, ino: u64) -> Result<(), u64> {
    let mut stat = [0_u8; 128];
    stat[8..16].copy_from_slice(&ino.to_le_bytes()); // st_ino
    stat[16..20].copy_from_slice(&mode.to_le_bytes()); // st_mode
    stat[20..24].copy_from_slice(&1_u32.to_le_bytes()); // st_nlink
    stat[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    stat[56..60].copy_from_slice(&1024_u32.to_le_bytes()); // st_blksize
    stat[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // st_blocks
    user_memory::copy_to_user(&stat, buffer).map_err(|_| EFAULT)
}

const ENOENT: u64 = 2;
const EBADF: u64 = 9;
const EAGAIN: u64 = 11;
const EFAULT: u64 = 14;
const ENOTDIR: u64 = 20;
const EISDIR: u64 = 21;
const EINVAL: u64 = 22;
const ENOSPC: u64 = 28;
const ESPIPE: u64 = 29;
const EROFS: u64 = 30;
const EPIPE: u64 = 32;
const EMFILE: u64 = 24;

const S_IFCHR: u32 = 0o020000;
const S_IFIFO: u32 = 0o010000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Current task id helper used by `linux_abi`.
pub fn current_task() -> u64 {
    scheduler::current_task_id()
}
