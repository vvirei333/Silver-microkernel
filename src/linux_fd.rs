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

/// A contiguous physical run backing a `MAP_SHARED` memfd. The run is owned
/// by the memfd, not by any address space: unmapping a task's window must not
/// free it, and a second mapping (another process) must see the same bytes.
struct MemFdFrames {
    physical_start: usize,
    page_count: usize,
    generation: u32,
}

/// Shared state of one `memfd_create` file. The offset lives in the shared
/// object, so `dup` shares it exactly like a real file description.
pub struct MemFdState {
    data: Vec<u8>,
    offset: usize,
    /// Populated by the first writable `MAP_SHARED` mapping. While present,
    /// file I/O goes through these frames so mappings and `read`/`write` see
    /// one copy.
    frames: Option<MemFdFrames>,
    /// Number of live mappings. Reallocating the run is only safe at zero.
    maps: usize,
}

impl MemFdState {
    fn len(&self) -> usize {
        self.data.len()
    }

    /// Ensures a physical run covers `needed` bytes. Grows only when no
    /// mapping exists, because a resize would move frames out from under an
    /// existing PTE.
    fn ensure_frames(&mut self, needed: usize) -> Result<(), u64> {
        let needed = needed.max(self.data.len());
        let page_count = needed.div_ceil(crate::memory::PAGE_SIZE).max(1);
        let capacity = self
            .frames
            .as_ref()
            .map(|frames| frames.page_count * crate::memory::PAGE_SIZE)
            .unwrap_or(0);
        if self.frames.is_some() && capacity >= needed {
            if self.data.len() < needed {
                self.data.resize(needed, 0);
            }
            return Ok(());
        }
        if self.frames.is_some() && self.maps != 0 {
            return Err(ENOMEM);
        }
        let Some((physical_start, generation)) =
            crate::frame_allocator::alloc_user_contiguous_pages_tagged(page_count)
        else {
            return Err(ENOMEM);
        };
        let base = crate::memory::phys_to_virt(physical_start);
        unsafe {
            core::ptr::write_bytes(base as *mut u8, 0, page_count * crate::memory::PAGE_SIZE);
            let copy = self.data.len().min(page_count * crate::memory::PAGE_SIZE);
            core::ptr::copy_nonoverlapping(self.data.as_ptr(), base as *mut u8, copy);
        }
        if let Some(old) = self.frames.take() {
            crate::frame_allocator::free_contiguous_pages_tagged(
                old.physical_start,
                old.page_count,
                old.generation,
            );
        }
        self.frames = Some(MemFdFrames {
            physical_start,
            page_count,
            generation,
        });
        if self.data.len() < needed {
            self.data.resize(needed, 0);
        }
        Ok(())
    }
}

impl Drop for MemFdState {
    fn drop(&mut self) {
        if let Some(frames) = self.frames.take() {
            crate::frame_allocator::free_contiguous_pages_tagged(
                frames.physical_start,
                frames.page_count,
                frames.generation,
            );
        }
    }
}

/// A live `MAP_SHARED` window on a memfd. Holding this keeps the memfd state
/// (and therefore its frames) alive even after every descriptor is closed, as
/// Linux does for a mapping.
pub struct MappedFile {
    state: Arc<Mutex<MemFdState>>,
}

impl Clone for MappedFile {
    fn clone(&self) -> Self {
        self.state.lock().maps += 1;
        Self {
            state: self.state.clone(),
        }
    }
}

impl MappedFile {
    /// Splits the shared run into the physical base and page count a mapping
    /// needs. `length` and `offset` are page-aligned by the caller.
    pub fn window(&self, offset: usize, length: usize) -> Result<(usize, usize), u64> {
        let mut state = self.state.lock();
        state.ensure_frames(offset.saturating_add(length))?;
        let frames = state.frames.as_ref().ok_or(ENOMEM)?;
        if offset.saturating_add(length) > frames.page_count * crate::memory::PAGE_SIZE {
            return Err(ENOMEM);
        }
        Ok((
            frames.physical_start + offset,
            length.div_ceil(crate::memory::PAGE_SIZE),
        ))
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.maps = state.maps.saturating_sub(1);
    }
}

/// One queued datagram/message on a Unix socket, with the descriptors that
/// travelled with it. Ancillary descriptors stay in the queue until the
/// receiver consumes the message, exactly like Linux.
struct SocketMessage {
    data: VecDeque<u8>,
    fds: Vec<Target>,
}

/// One end of a connected `AF_UNIX` stream pair.
#[derive(Default)]
struct SocketEnd {
    incoming: VecDeque<SocketMessage>,
    /// This end called `shutdown(SHUT_WR)` or was closed: the peer reads EOF.
    write_closed: bool,
    /// This end will not read again: writes from the peer fail with `EPIPE`.
    read_closed: bool,
}

/// Shared state of a `socketpair(2)`. `refs` counts the live descriptors
/// naming each end, so closing the last one wakes the peer with EOF. The two
/// queues are directional: end 0 writes into `ends[1].incoming`.
struct SocketState {
    ends: [SocketEnd; 2],
    refs: [usize; 2],
    stream: bool,
}

/// One descriptor registered with an epoll instance.
struct EpollEntry {
    fd: u64,
    events: u32,
    data: u64,
}

struct EpollState {
    entries: Vec<EpollEntry>,
}

/// A `timerfd_create` timer. Deadlines are in scheduler ticks (100 Hz).
struct TimerFdState {
    /// Zero means disarmed.
    deadline: u64,
    interval: u64,
}

/// State of a bound `AF_UNIX` listening socket. The registry holds one Arc
/// per bound path; the fd table holds another. Pending entries own the server
/// end of each accepted-not-yet-accepted connection.
struct ListenerState {
    path: Vec<u8>,
    refs: usize,
    listening: bool,
    backlog: usize,
    pending: VecDeque<Arc<Mutex<SocketState>>>,
}

static LISTENERS: Mutex<Vec<Arc<Mutex<ListenerState>>>> = Mutex::new(Vec::new());

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
    Socket {
        state: Arc<Mutex<SocketState>>,
        end: u8,
        nonblock: bool,
    },
    /// A socket created by `socket(2)` but not connected or bound yet.
    Unbound {
        nonblock: bool,
    },
    /// A bound `AF_UNIX` socket; `listen(2)` flips it to accepting.
    Listener {
        state: Arc<Mutex<ListenerState>>,
        nonblock: bool,
    },
    Epoll(Arc<Mutex<EpollState>>),
    TimerFd {
        state: Arc<Mutex<TimerFdState>>,
        nonblock: bool,
    },
}

struct Table {
    task: u64,
    slots: Vec<Option<Target>>,
}

static TABLES: Mutex<Vec<Table>> = Mutex::new(Vec::new());

const MAX_FDS: usize = 256;

fn with_table<R>(task: u64, action: impl FnOnce(&mut Table) -> R) -> R {
    let task = crate::scheduler::fd_table_owner(task);
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

/// Drops one reference to one end of a connected pair. The last reference
/// closes that side and wakes the peer with EOF.
fn release_socket_end(state: &Arc<Mutex<SocketState>>, end: u8) {
    let dropped = {
        let mut socket = state.lock();
        let end = end as usize;
        if socket.refs[end] == 0 {
            return;
        }
        socket.refs[end] -= 1;
        if socket.refs[end] != 0 {
            return;
        }
        socket.ends[end].write_closed = true;
        socket.ends[end].read_closed = true;
        socket.ends[end].incoming.drain(..).collect::<Vec<_>>()
    };
    for message in dropped {
        for target in message.fds {
            target.release_endpoint();
        }
    }
}

impl Target {
    /// A new descriptor referring to the same pipe end.
    fn retain_endpoint(&self) {
        match self {
            Target::Pipe { state, read } => {
                let mut pipe = state.lock();
                if *read {
                    pipe.read_ends += 1;
                } else {
                    pipe.write_ends += 1;
                }
            }
            Target::Socket { state, end, .. } => {
                state.lock().refs[*end as usize] += 1;
            }
            Target::Listener { state, .. } => {
                state.lock().refs += 1;
            }
            _ => {}
        }
    }

    /// One descriptor of this pipe end went away.
    fn release_endpoint(&self) {
        match self {
            Target::Pipe { state, read } => {
                let mut pipe = state.lock();
                if *read {
                    pipe.read_ends = pipe.read_ends.saturating_sub(1);
                } else {
                    pipe.write_ends = pipe.write_ends.saturating_sub(1);
                }
            }
            Target::Socket { state, end, .. } => {
                release_socket_end(state, *end);
            }
            Target::Listener { state, .. } => {
                // Lock the registry before the listener, matching `bind`, so
                // the two lock orders cannot deadlock.
                let mut registry = LISTENERS.lock();
                let mut listener = state.lock();
                if listener.refs == 0 {
                    return;
                }
                listener.refs -= 1;
                if listener.refs != 0 {
                    return;
                }
                let pending: Vec<Arc<Mutex<SocketState>>> =
                    listener.pending.drain(..).collect();
                let path = listener.path.clone();
                drop(listener);
                registry.retain(|entry| entry.lock().path != path);
                drop(registry);
                for connection in pending {
                    release_socket_end(&connection, 1);
                }
            }
            _ => {}
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
    let task = crate::scheduler::fd_table_owner(task);
    const O_WRONLY: u64 = 0o1;
    const O_RDWR: u64 = 0o2;
    const O_CREAT: u64 = 0o100;
    const O_EXCL: u64 = 0o200;
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
    if !crate::scheduler::read_path_allowed(task, &resolved) {
        return Err(EACCES);
    }
    let wants_write =
        flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND) != 0;
    if wants_write {
        // SFS is read-only; only the RamFs scratch tree accepts writes.
        if !silver_microkernel::path_is_under(&resolved, crate::scheduler::LINUX_SCRATCH_PREFIX)
        {
            return Err(EROFS);
        }
        let exists = vfs::file_len(cwd, path).is_ok();
        if flags & O_CREAT != 0 && flags & O_EXCL != 0 && exists {
            return Err(EEXIST);
        }
        if flags & O_TRUNC != 0 {
            vfs::create(cwd, path).map_err(|_| EACCES)?;
        } else if !exists {
            if flags & O_CREAT == 0 {
                return Err(ENOENT);
            }
            vfs::create(cwd, path).map_err(|_| EACCES)?;
        }
        let vfs_flags = crate::vfs::OPEN_WRITE
            | if flags & O_APPEND != 0 {
                crate::vfs::OPEN_APPEND
            } else {
                0
            };
        let fd = vfs::open(task, cwd, path, vfs_flags).map_err(|_| EACCES)?;
        return insert(task, Target::Vfs(fd)).ok_or(EMFILE);
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
    let task = crate::scheduler::fd_table_owner(task);
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
    let task = crate::scheduler::fd_table_owner(task);
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
        Target::Socket { state, end, nonblock } => {
            let mut chunk = alloc::vec![0_u8; (count as usize).min(64 * 1024)];
            let (read, fds) = socket_recv_message(&state, end, nonblock, &mut chunk)?;
            for target in fds {
                let _ = insert_transferred(task, target);
            }
            user_memory::copy_to_user(&chunk[..read], buffer).map_err(|_| EFAULT)?;
            Ok(read as u64)
        }
        Target::Unbound { .. } | Target::Listener { .. } => Err(ENOTCONN),
        Target::TimerFd { state, nonblock } => timerfd_read(&state, nonblock, buffer),
        Target::Epoll(_) => Err(EINVAL),
    }
}

/// Positional read used by file-backed `mmap`. Does not move the fd offset.
pub fn pread(task: u64, fd: u64, offset: u64, buffer: &mut [u8]) -> Result<usize, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        Target::Vfs(vfs_fd) => {
            if vfs::fd_kind(task, vfs_fd) == Some(vfs::FD_KIND_DIR) {
                return Err(EISDIR);
            }
            let mut read = 0usize;
            vfs::pread_with(
                task,
                vfs_fd,
                offset as usize,
                buffer.len(),
                |bytes| {
                    let count = bytes.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&bytes[..count]);
                    read = count;
                    Ok(())
                },
            )
            .map_err(|_| EBADF)?;
            Ok(read)
        }
        Target::MemFd(state) => {
            let file = state.lock();
            let start = (offset as usize).min(file.len());
            let count = (file.len() - start).min(buffer.len());
            if let Some(frames) = &file.frames {
                let source = crate::memory::phys_to_virt(frames.physical_start + start);
                let bytes = unsafe { core::slice::from_raw_parts(source as *const u8, count) };
                buffer[..count].copy_from_slice(bytes);
            } else {
                buffer[..count].copy_from_slice(&file.data[start..start + count]);
            }
            Ok(count)
        }
        Target::Zero => {
            let count = buffer.len();
            buffer.fill(0);
            Ok(count)
        }
        Target::Null => Ok(0),
        _ => Err(ENODEV),
    }
}

pub fn write(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
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
        Target::Socket { state, end, .. } => {
            let mut bytes = alloc::vec![0_u8; (count as usize).min(1 << 20)];
            user_memory::copy_from_user(buffer, &mut bytes).map_err(|_| EFAULT)?;
            socket_send_locked(&state, end, &bytes, Vec::new())
        }
        Target::Unbound { .. } | Target::Listener { .. } => Err(ENOTCONN),
        Target::TimerFd { .. } | Target::Epoll(_) => Err(EINVAL),
    }
}

pub fn lseek(task: u64, fd: u64, offset: i64, whence: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
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

/// Positional write used by `pwrite64`. Grows a memfd like `write` does and
/// does not move the descriptor offset.
pub fn pwrite(task: u64, fd: u64, offset: u64, bytes: &[u8]) -> Result<usize, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let mut file = state.lock();
    let start = offset as usize;
    let end = start.checked_add(bytes.len()).ok_or(EINVAL)?;
    if file.frames.is_some() {
        let capacity = file
            .frames
            .as_ref()
            .map(|frames| frames.page_count * crate::memory::PAGE_SIZE)
            .unwrap_or(0);
        if end > capacity {
            if file.maps != 0 {
                return Err(EFBIG);
            }
            file.ensure_frames(end)?;
        }
        let physical = file.frames.as_ref().ok_or(ENOMEM)?.physical_start;
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                crate::memory::phys_to_virt(physical + start) as *mut u8,
                bytes.len(),
            );
        }
    } else {
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
        file.data[start..end].copy_from_slice(bytes);
    }
    if end > file.data.len() {
        file.data.resize(end, 0);
    }
    Ok(bytes.len())
}

/// Fills the aarch64 `struct stat` for a descriptor.
pub fn fstat(task: u64, fd: u64, buffer: u64) -> Result<(), u64> {
    let task = crate::scheduler::fd_table_owner(task);
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
        Target::Socket { .. } | Target::Unbound { .. } | Target::Listener { .. } => {
            write_stat(buffer, S_IFSOCK | 0o600, 0, fd.max(1))
        }
        Target::Epoll(_) | Target::TimerFd { .. } => {
            write_stat(buffer, S_IFREG | 0o600, 0, fd.max(1))
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
    let task = crate::scheduler::fd_table_owner(task);
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
            frames: None,
            maps: 0,
        }))),
    )
    .ok_or(EMFILE)
}

/// `epoll_create1(2)`.
pub fn epoll_create(task: u64) -> Result<u64, u64> {
    insert(
        task,
        Target::Epoll(Arc::new(Mutex::new(EpollState {
            entries: Vec::new(),
        }))),
    )
    .ok_or(EMFILE)
}

/// `epoll_ctl(2)`: add (`1`), delete (`2`) or modify (`3`) one interest.
pub fn epoll_ctl(
    task: u64,
    epfd: u64,
    operation: u64,
    fd: u64,
    events: u32,
    data: u64,
) -> Result<(), u64> {
    let target = lookup(task, epfd).ok_or(EBADF)?;
    let Target::Epoll(state) = target else {
        return Err(EINVAL);
    };
    if lookup(task, fd).is_none() {
        return Err(EBADF);
    }
    let mut epoll = state.lock();
    match operation {
        1 => {
            if epoll.entries.iter().any(|entry| entry.fd == fd) {
                return Err(EEXIST);
            }
            if epoll.entries.len() >= 64 {
                return Err(ENOSPC);
            }
            epoll.entries.push(EpollEntry { fd, events, data });
            Ok(())
        }
        2 => {
            let Some(index) = epoll.entries.iter().position(|entry| entry.fd == fd) else {
                return Err(ENOENT);
            };
            epoll.entries.remove(index);
            Ok(())
        }
        3 => {
            let Some(entry) = epoll.entries.iter_mut().find(|entry| entry.fd == fd) else {
                return Err(ENOENT);
            };
            entry.events = events;
            entry.data = data;
            Ok(())
        }
        _ => Err(EINVAL),
    }
}

/// Level-triggered readiness translated from the poll bits.
fn epoll_ready_mask(task: u64, fd: u64, requested: u32) -> u32 {
    const EPOLLIN: u32 = 0x001;
    const EPOLLOUT: u32 = 0x004;
    const EPOLLERR: u32 = 0x008;
    const EPOLLHUP: u32 = 0x010;
    let Some(bits) = poll_ready(task, fd) else {
        return 0;
    };
    let mut ready = 0;
    if bits & 0x001 != 0 && requested & EPOLLIN != 0 {
        ready |= EPOLLIN;
    }
    if bits & 0x004 != 0 && requested & EPOLLOUT != 0 {
        ready |= EPOLLOUT;
    }
    if bits & 0x008 != 0 {
        ready |= EPOLLERR;
    }
    if bits & 0x010 != 0 {
        ready |= EPOLLHUP;
    }
    ready
}

/// `epoll_wait(2)` / `epoll_pwait(2)`. Blocks until one entry is ready or the
/// timeout expires. A negative timeout means "infinite", capped like `ppoll`
/// so a stuck client cannot wedge a task forever.
pub fn epoll_wait(
    task: u64,
    epfd: u64,
    max_events: usize,
    timeout_ms: i64,
) -> Result<Vec<(u32, u64)>, u64> {
    let target = lookup(task, epfd).ok_or(EBADF)?;
    let Target::Epoll(state) = target else {
        return Err(EINVAL);
    };
    let start = crate::timer::ticks();
    let deadline = if timeout_ms < 0 {
        Some(start + crate::timer::milliseconds_to_ticks(60_000))
    } else {
        Some(start + crate::timer::milliseconds_to_ticks(timeout_ms as u64))
    };
    loop {
        let mut ready = Vec::new();
        {
            let epoll = state.lock();
            for entry in &epoll.entries {
                if ready.len() >= max_events {
                    break;
                }
                let mask = epoll_ready_mask(task, entry.fd, entry.events);
                if mask != 0 {
                    ready.push((mask, entry.data));
                }
            }
        }
        let expired = deadline.map(|deadline| crate::timer::ticks() >= deadline);
        if !ready.is_empty() || expired == Some(true) || timeout_ms == 0 {
            return Ok(ready);
        }
        scheduler::sleep(1);
    }
}

/// `timerfd_create(2)`.
pub fn timerfd_create(task: u64, nonblock: bool) -> Result<u64, u64> {
    insert(
        task,
        Target::TimerFd {
            state: Arc::new(Mutex::new(TimerFdState {
                deadline: 0,
                interval: 0,
            })),
            nonblock,
        },
    )
    .ok_or(EMFILE)
}

/// Arms a timerfd. `interval` and `value` are in scheduler ticks; `absolute`
/// means `value` is a CLOCK_MONOTONIC deadline rather than a delay.
pub fn timerfd_settime(
    task: u64,
    fd: u64,
    interval: u64,
    value: u64,
    absolute: bool,
) -> Result<(), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::TimerFd { state, .. } = target else {
        return Err(EINVAL);
    };
    let mut timer = state.lock();
    timer.interval = interval;
    timer.deadline = if value == 0 {
        0
    } else if absolute {
        value
    } else {
        crate::timer::ticks().saturating_add(value)
    };
    Ok(())
}

/// Returns `(interval, remaining)` in scheduler ticks.
pub fn timerfd_gettime(task: u64, fd: u64) -> Result<(u64, u64), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::TimerFd { state, .. } = target else {
        return Err(EINVAL);
    };
    let timer = state.lock();
    let remaining = if timer.deadline == 0 {
        0
    } else {
        timer.deadline.saturating_sub(crate::timer::ticks())
    };
    Ok((timer.interval, remaining))
}

fn timerfd_read(
    state: &Arc<Mutex<TimerFdState>>,
    nonblock: bool,
    buffer: u64,
) -> Result<u64, u64> {
    loop {
        let expired = {
            let mut timer = state.lock();
            let now = crate::timer::ticks();
            if timer.deadline != 0 && now >= timer.deadline {
                let count = if timer.interval != 0 {
                    1 + (now - timer.deadline) / timer.interval
                } else {
                    1
                };
                if timer.interval != 0 {
                    timer.deadline = timer.deadline.saturating_add(count * timer.interval);
                } else {
                    timer.deadline = 0;
                }
                Some(count)
            } else {
                None
            }
        };
        if let Some(count) = expired {
            user_memory::copy_to_user(&count.to_le_bytes(), buffer).map_err(|_| EFAULT)?;
            return Ok(8);
        }
        if nonblock {
            return Err(EAGAIN);
        }
        scheduler::sleep(1);
    }
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
    let available = file.len().saturating_sub(file.offset);
    let take = (count as usize).min(available);
    if take == 0 {
        return Ok(0);
    }
    let chunk = if let Some(frames) = &file.frames {
        let source = crate::memory::phys_to_virt(frames.physical_start + file.offset);
        unsafe { core::slice::from_raw_parts(source as *const u8, take) }.to_vec()
    } else {
        file.data[file.offset..file.offset + take].to_vec()
    };
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
    if file.frames.is_some() {
        // A live mapping fixes the run in place; growing it would move the
        // frames. Fail instead of silently writing into unmapped memory.
        let capacity = file
            .frames
            .as_ref()
            .map(|frames| frames.page_count * crate::memory::PAGE_SIZE)
            .unwrap_or(0);
        if end > capacity {
            if file.maps != 0 {
                return Err(EFBIG);
            }
            file.ensure_frames(end)?;
        }
        let physical = file.frames.as_ref().ok_or(ENOMEM)?.physical_start;
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                crate::memory::phys_to_virt(physical + start) as *mut u8,
                total,
            );
        }
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
    } else {
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
        file.data[start..end].copy_from_slice(&bytes);
    }
    file.offset = end;
    Ok(total as u64)
}

/// `ftruncate(2)` / `fallocate(2)` on a memfd. The file is backed lazily; a
/// resize is refused while a shared mapping pins the current run.
pub fn memfd_truncate(task: u64, fd: u64, length: usize) -> Result<(), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let mut file = state.lock();
    if length > file.data.len() {
        if file.frames.is_some() {
            let capacity = file
                .frames
                .as_ref()
                .map(|frames| frames.page_count * crate::memory::PAGE_SIZE)
                .unwrap_or(0);
            if length > capacity {
                if file.maps != 0 {
                    return Err(ENOMEM);
                }
                file.ensure_frames(length)?;
            }
        }
    }
    file.data.resize(length, 0);
    Ok(())
}

/// `fallocate(2)` with mode 0: grow the file if `offset + length` exceeds it.
pub fn memfd_fallocate(task: u64, fd: u64, offset: usize, length: usize) -> Result<(), u64> {
    let needed = offset.checked_add(length).ok_or(EINVAL)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let current = state.lock().data.len();
    if needed > current {
        memfd_truncate(task, fd, needed)?;
    }
    Ok(())
}

/// Maps a memfd `MAP_SHARED` window. Returns the physical base of the window
/// and a handle that keeps the memfd alive for the lifetime of the mapping.
pub fn memfd_shared(
    task: u64,
    fd: u64,
    offset: usize,
    length: usize,
) -> Result<(usize, MappedFile), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    state.lock().ensure_frames(offset.saturating_add(length))?;
    state.lock().maps += 1;
    let owner = MappedFile {
        state: state.clone(),
    };
    let (physical, _) = owner.window(offset, length)?;
    Ok((physical, owner))
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

/// Moves a descriptor received with `SCM_RIGHTS` into `task`'s table. The
/// caller passes a target that already owns one reference (the message
/// queue); on success the table owns it, on failure that reference is
/// released.
fn insert_transferred(task: u64, target: Target) -> Option<u64> {
    match insert(task, target.clone()) {
        Some(fd) => {
            // The table's copy becomes a counted endpoint, then the queue's
            // reference is released: exactly one owner remains.
            target.retain_endpoint();
            target.release_endpoint();
            Some(fd)
        }
        None => {
            target.release_endpoint();
            None
        }
    }
}

/// Creates an unnamed `AF_UNIX` stream pair (`socketpair(2)`).
pub fn socketpair(task: u64, nonblock: bool) -> Result<(u64, u64), u64> {
    let state = Arc::new(Mutex::new(SocketState {
        ends: [SocketEnd::default(), SocketEnd::default()],
        refs: [1, 1],
        stream: true,
    }));
    let first = Target::Socket {
        state: state.clone(),
        end: 0,
        nonblock,
    };
    let second = Target::Socket {
        state,
        end: 1,
        nonblock,
    };
    let Some(first_fd) = insert(task, first.clone()) else {
        first.release_endpoint();
        second.release_endpoint();
        return Err(EMFILE);
    };
    match insert(task, second.clone()) {
        Some(second_fd) => Ok((first_fd, second_fd)),
        None => {
            second.release_endpoint();
            let _ = close(task, first_fd);
            Err(EMFILE)
        }
    }
}

/// `socket(2)` for `AF_UNIX`: an unbound endpoint, ready for `bind` or
/// `connect`.
pub fn socket(task: u64, nonblock: bool) -> Result<u64, u64> {
    insert(task, Target::Unbound { nonblock }).ok_or(EMFILE)
}

/// Replaces exactly one descriptor slot, optionally requiring it to still be
/// an unbound socket. Used by `bind` and `connect`, which turn the calling
/// descriptor into a different kind of endpoint.
fn replace_unbound(task: u64, fd: u64, replacement: Target) -> Result<(), u64> {
    with_table(task, |table| {
        let Some(slot) = table.slots.get_mut(fd as usize) else {
            return false;
        };
        if !matches!(slot, Some(Target::Unbound { .. })) {
            return false;
        }
        *slot = Some(replacement);
        true
    })
    .then_some(())
    .ok_or(EBADF)
}

/// `bind(2)` for a filesystem or abstract `AF_UNIX` path.
pub fn bind(task: u64, fd: u64, path: Vec<u8>) -> Result<(), u64> {
    if path.is_empty() {
        return Err(EINVAL);
    }
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Unbound { nonblock } = target else {
        return Err(EINVAL);
    };
    let state = Arc::new(Mutex::new(ListenerState {
        path: path.clone(),
        refs: 1,
        listening: false,
        backlog: 1,
        pending: VecDeque::new(),
    }));
    {
        let mut registry = LISTENERS.lock();
        if registry
            .iter()
            .any(|entry| entry.lock().path == path)
        {
            return Err(EADDRINUSE);
        }
        registry.push(state.clone());
    }
    if replace_unbound(
        task,
        fd,
        Target::Listener {
            state: state.clone(),
            nonblock,
        },
    )
    .is_err()
    {
        LISTENERS.lock().retain(|entry| !Arc::ptr_eq(entry, &state));
        return Err(EBADF);
    }
    Ok(())
}

/// `listen(2)`: marks a bound socket as accepting connections.
pub fn listen(task: u64, fd: u64, backlog: u64) -> Result<(), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Listener { state, .. } = target else {
        return Err(EINVAL);
    };
    let mut listener = state.lock();
    listener.listening = true;
    listener.backlog = (backlog as usize).clamp(1, 128);
    Ok(())
}

/// `connect(2)`: joins a bound listener and turns the calling descriptor into
/// the client end of a connected pair.
pub fn connect(task: u64, fd: u64, path: Vec<u8>) -> Result<(), u64> {
    if path.is_empty() {
        return Err(EINVAL);
    }
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Unbound { nonblock } = target else {
        return Err(EINVAL);
    };
    let listener = {
        let registry = LISTENERS.lock();
        registry
            .iter()
            .find(|entry| entry.lock().path == path)
            .cloned()
    };
    let Some(listener) = listener else {
        return Err(ECONNREFUSED);
    };
    let pair = Arc::new(Mutex::new(SocketState {
        ends: [SocketEnd::default(), SocketEnd::default()],
        refs: [1, 1],
        stream: true,
    }));
    {
        let mut state = listener.lock();
        if !state.listening || state.pending.len() >= state.backlog.max(1) {
            return Err(ECONNREFUSED);
        }
        // The queue takes ownership of the server end's reference.
        state.pending.push_back(pair.clone());
    }
    if replace_unbound(
        task,
        fd,
        Target::Socket {
            state: pair.clone(),
            end: 0,
            nonblock,
        },
    )
    .is_err()
    {
        {
            let mut state = listener.lock();
            state
                .pending
                .retain(|connection| !Arc::ptr_eq(connection, &pair));
        }
        release_socket_end(&pair, 0);
        release_socket_end(&pair, 1);
        return Err(EBADF);
    }
    Ok(())
}

/// `accept(2)`/`accept4(2)`: pops one pending connection. Returns the new
/// descriptor; the caller writes the (unnamed) peer address.
pub fn accept(task: u64, fd: u64, nonblock_flag: bool) -> Result<u64, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Listener { state, nonblock } = target else {
        return Err(EINVAL);
    };
    let nonblock = nonblock || nonblock_flag;
    loop {
        let pending = {
            let mut listener = state.lock();
            listener.pending.pop_front()
        };
        if let Some(pair) = pending {
            // The queue's reference to the server end moves into the table.
            match insert(
                task,
                Target::Socket {
                    state: pair.clone(),
                    end: 1,
                    nonblock,
                },
            ) {
                Some(new_fd) => return Ok(new_fd),
                None => {
                    release_socket_end(&pair, 1);
                    return Err(EMFILE);
                }
            }
        }
        if nonblock {
            return Err(EAGAIN);
        }
        scheduler::sleep(1);
    }
}

/// True when `fd` names any kind of Unix socket.
pub fn is_socket(task: u64, fd: u64) -> bool {
    matches!(
        lookup(task, fd),
        Some(Target::Socket { .. } | Target::Unbound { .. } | Target::Listener { .. })
    )
}

/// Local address bytes for `getsockname`; `None` means an unnamed pair.
pub fn socket_local_name(task: u64, fd: u64) -> Option<Vec<u8>> {
    match lookup(task, fd)? {
        Target::Listener { state, .. } => Some(state.lock().path.clone()),
        _ => None,
    }
}

/// Queues `bytes` (and any passed descriptors) on the peer end.
fn socket_send_locked(
    state: &Arc<Mutex<SocketState>>,
    end: u8,
    bytes: &[u8],
    fds: Vec<Target>,
) -> Result<u64, u64> {
    let end = end as usize;
    let peer = 1 - end;
    let mut socket = state.lock();
    if socket.ends[end].write_closed || socket.ends[peer].read_closed {
        drop(socket);
        for target in &fds {
            target.release_endpoint();
        }
        return Err(EPIPE);
    }
    if !bytes.is_empty() || !fds.is_empty() {
        socket.ends[peer].incoming.push_back(SocketMessage {
            data: VecDeque::from(bytes.to_vec()),
            fds,
        });
    }
    drop(socket);
    Ok(bytes.len() as u64)
}

/// `sendmsg(2)` core: copies `pass` descriptors from the sender's table into
/// the message. The caller supplies the already gathered payload.
pub fn socket_send(
    task: u64,
    fd: u64,
    bytes: &[u8],
    pass: &[u64],
    _nonblock: bool,
) -> Result<u64, u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Socket { state, end, .. } = target else {
        return Err(ENOTSOCK);
    };
    let mut passed: Vec<Target> = Vec::new();
    for &passed_fd in pass {
        let Some(passed_target) = lookup(task, passed_fd) else {
            for target in &passed {
                target.release_endpoint();
            }
            return Err(EBADF);
        };
        passed_target.retain_endpoint();
        passed.push(passed_target);
    }
    socket_send_locked(&state, end, bytes, passed)
}

/// Pops one message, filling `buffer`. Returns the byte count and any
/// descriptors that were attached. A short buffer keeps the remainder and
/// the descriptors stay queued until the message is fully consumed.
fn socket_recv_message(
    state: &Arc<Mutex<SocketState>>,
    end: u8,
    nonblock: bool,
    buffer: &mut [u8],
) -> Result<(usize, Vec<Target>), u64> {
    let end = end as usize;
    let peer = 1 - end;
    loop {
        let mut socket = state.lock();
        if socket.ends[end].read_closed {
            return Ok((0, Vec::new()));
        }
        if let Some(message) = socket.ends[end].incoming.front_mut() {
            let take = buffer.len().min(message.data.len());
            for slot in buffer.iter_mut().take(take) {
                *slot = message.data.pop_front().unwrap();
            }
            let fds = if message.data.is_empty() {
                message.fds.drain(..).collect()
            } else {
                Vec::new()
            };
            if message.data.is_empty() {
                socket.ends[end].incoming.pop_front();
            }
            return Ok((take, fds));
        }
        if socket.ends[peer].write_closed {
            return Ok((0, Vec::new()));
        }
        drop(socket);
        if nonblock {
            return Err(EAGAIN);
        }
        scheduler::sleep(1);
    }
}

/// `recvmsg(2)` core: returns the payload length and installs received
/// descriptors into `task`'s table under fresh numbers.
pub fn socket_recv(
    task: u64,
    fd: u64,
    buffer: &mut [u8],
    nonblock_hint: bool,
) -> Result<(usize, Vec<u64>), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Socket {
        state,
        end,
        nonblock,
    } = target
    else {
        return Err(ENOTSOCK);
    };
    let (read, fds) = socket_recv_message(&state, end, nonblock || nonblock_hint, buffer)?;
    let mut numbers = Vec::new();
    for target in fds {
        if let Some(number) = insert_transferred(task, target) {
            numbers.push(number);
        }
    }
    Ok((read, numbers))
}

/// `shutdown(2)`. `how`: 0 read, 1 write, 2 both.
pub fn socket_shutdown(task: u64, fd: u64, how: u64) -> Result<(), u64> {
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Socket { state, end, .. } = target else {
        return Err(ENOTSOCK);
    };
    let end = end as usize;
    let mut socket = state.lock();
    let dropped = match how {
        0 => {
            socket.ends[end].read_closed = true;
            socket.ends[end].incoming.drain(..).collect::<Vec<_>>()
        }
        1 => {
            socket.ends[end].write_closed = true;
            Vec::new()
        }
        2 => {
            socket.ends[end].read_closed = true;
            socket.ends[end].write_closed = true;
            socket.ends[end].incoming.drain(..).collect::<Vec<_>>()
        }
        _ => return Err(EINVAL),
    };
    drop(socket);
    for message in dropped {
        for target in message.fds {
            target.release_endpoint();
        }
    }
    Ok(())
}

/// Open-file status flags for `fcntl(F_GETFL)`.
pub fn status_flags(task: u64, fd: u64) -> Option<u64> {
    const O_WRONLY: u64 = 1;
    const O_RDWR: u64 = 2;
    const O_NONBLOCK: u64 = 0o4000;
    let target = lookup(task, fd)?;
    let (access, nonblock) = match target {
        Target::Vfs(_) => (0, false),
        Target::Pipe { read, .. } => (if read { 0 } else { O_WRONLY }, false),
        Target::Socket { nonblock, .. }
        | Target::Unbound { nonblock }
        | Target::Listener { nonblock, .. }
        | Target::EventFd { nonblock, .. } => (O_RDWR, nonblock),
        Target::TimerFd { nonblock, .. } => (O_RDWR, nonblock),
        Target::Console
        | Target::Null
        | Target::Zero
        | Target::Full
        | Target::Random
        | Target::MemFd(_)
        | Target::Epoll(_) => (O_RDWR, false),
    };
    Some(access | if nonblock { O_NONBLOCK } else { 0 })
}

/// `fcntl(F_SETFL)` for the descriptors that can actually block.
pub fn set_nonblock(task: u64, fd: u64, nonblock: bool) -> Result<(), u64> {
    with_table(task, |table| {
        match table
            .slots
            .get_mut(fd as usize)
            .and_then(|slot| slot.as_mut())
        {
            Some(Target::Socket { nonblock: flag, .. })
            | Some(Target::Unbound { nonblock: flag })
            | Some(Target::Listener { nonblock: flag, .. })
            | Some(Target::TimerFd { nonblock: flag, .. })
            | Some(Target::EventFd { nonblock: flag, .. }) => {
                *flag = nonblock;
                true
            }
            _ => false,
        }
    })
    .then_some(())
    .ok_or(EINVAL)
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
        Target::Socket { state, end, .. } => {
            let socket = state.lock();
            let end = end as usize;
            let peer = 1 - end;
            let mut ready = 0;
            if !socket.ends[end].incoming.is_empty() {
                ready |= POLLIN;
            }
            if socket.ends[end].read_closed {
                ready |= POLLHUP;
            } else if socket.ends[peer].write_closed {
                // Drained queue plus a closed writer: EOF, still readable.
                ready |= POLLIN | POLLHUP;
            }
            if !socket.ends[end].write_closed && !socket.ends[peer].read_closed {
                ready |= POLLOUT;
            } else {
                ready |= POLLERR;
            }
            ready
        }
        Target::Unbound { .. } => POLLOUT,
        Target::Listener { state, .. } => {
            if state.lock().pending.is_empty() {
                0
            } else {
                POLLIN
            }
        }
        Target::TimerFd { state, .. } => {
            let timer = state.lock();
            if timer.deadline != 0 && crate::timer::ticks() >= timer.deadline {
                POLLIN
            } else {
                0
            }
        }
        Target::Epoll(_) => POLLIN | POLLOUT,
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
const EACCES: u64 = 13;
const EFAULT: u64 = 14;
const ENODEV: u64 = 19;
const ENOTDIR: u64 = 20;
const EISDIR: u64 = 21;
const EINVAL: u64 = 22;
const ENOSPC: u64 = 28;
const ESPIPE: u64 = 29;
const EROFS: u64 = 30;
const EPIPE: u64 = 32;
const ENOTSOCK: u64 = 88;
const ENOTCONN: u64 = 107;
const EADDRINUSE: u64 = 98;
const EEXIST: u64 = 17;
const ECONNREFUSED: u64 = 111;
const ENOMEM: u64 = 12;
const EFBIG: u64 = 27;
const EMFILE: u64 = 24;
const S_IFSOCK: u32 = 0o140000;

const S_IFCHR: u32 = 0o020000;
const S_IFIFO: u32 = 0o010000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Current task id helper used by `linux_abi`.
pub fn current_task() -> u64 {
    scheduler::current_task_id()
}
