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
    /// Kernel-heap shadow for small memfds. Large memfds are backed by
    /// `frames` and this is empty: keeping a full second copy of a 64 MiB
    /// Wayland shm pool exhausted the 32 MiB kernel heap and panicked.
    data: Vec<u8>,
    /// File size, independent of whether `data` still holds a copy.
    len: usize,
    offset: usize,
    /// Populated by the first writable `MAP_SHARED` mapping. While present,
    /// file I/O goes through these frames so mappings and `read`/`write` see
    /// one copy.
    frames: Option<MemFdFrames>,
    /// Number of live mappings. Reallocating the run is only safe at zero.
    maps: usize,
    /// `F_SEAL_*` bits added with `F_ADD_SEALS`. We do not enforce the seals
    /// (there is no unprivileged writer that could race), but toolkits check
    /// the call succeeds and read the mask back.
    seals: u32,
}

/// Memfds up to this size keep a heap shadow; larger ones are frame-backed
/// from the first write/truncate so the kernel heap stays bounded.
const MEMFD_SHADOW_LIMIT: usize = 1 << 20;

impl MemFdState {
    fn len(&self) -> usize {
        self.len
    }

    /// Ensures a physical run covers `needed` bytes. Grows only when no
    /// mapping exists, because a resize would move frames out from under an
    /// existing PTE.
    fn ensure_frames(&mut self, needed: usize) -> Result<(), u64> {
        let needed = needed.max(self.len);
        let page_count = needed.div_ceil(crate::memory::PAGE_SIZE).max(1);
        if let Some(frames) = &self.frames {
            if frames.page_count * crate::memory::PAGE_SIZE >= needed {
                self.len = needed;
                return Ok(());
            }
            if self.maps != 0 {
                return Err(ENOMEM);
            }
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
        // The frames are now the single copy of the content.
        self.data = Vec::new();
        self.len = needed;
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
/// A descriptor travelling through `SCM_RIGHTS`: the target plus the
/// sender's rights and ioctl allowlist, which must survive the transfer.
type PassedFd = (Target, u32, Option<Vec<u32>>);

struct SocketMessage {
    data: VecDeque<u8>,
    fds: Vec<PassedFd>,
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
    /// `EPOLLONESHOT` entries are disabled after one report until the next
    /// `EPOLL_CTL_MOD`. async-io/polling relies on this: without it an
    /// always-writable eventfd keeps its reactor spinning and the real
    /// socket is never serviced.
    armed: bool,
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
    /// Host-shared QEMU ivshmem window (`/dev/ivshmem`).
    Ivshmem,
    /// virtio-gpu scanout framebuffer (`/dev/gpufb`).
    GpuFb,
    /// DRM/KMS card (`/dev/dri/card0`), backed by virtio-gpu.
    Drm { render: bool },
    /// A file in the synthetic sysfs tree (DRM device description).
    Sysfs(&'static [u8], usize),
    /// A file in the synthetic procfs tree (read-only, process must be init).
    Proc(&'static [u8], usize),
    /// One evdev event device (`/dev/input/eventN`), backed by the kernel's
    /// virtio-input driver. The index selects the device.
    Input(usize),
    /// A synthetic directory listing (`/dev/dri`), with the next entry index.
    DirList {
        entries: &'static [&'static str],
        offset: usize,
    },
    /// A sysfs symlink opened with O_NOFOLLOW. readlinkat(fd, "") returns
    /// the target, which is how systemd reads subsystem/device links.
    SysfsLink(&'static str),
    /// A synthetic sysfs directory with typed entries. The path is kept so
    /// openat/readlinkat with this directory as dirfd can resolve children.
    SysfsDir {
        entries: &'static [crate::sysfs::Entry],
        path: String,
        offset: usize,
    },
    /// A buffer exported through PRIME (`/dev/dri` handle -> fd). Carries the
    /// physical range so any task that receives the fd can map the pixels.
    Prime { pa: usize, size: usize },
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
    /// `signalfd(2)`: signals routed here instead of the pending bit.
    SignalFd(Arc<Mutex<SignalFdState>>),
    /// AF_NETLINK socket (libudev's uevent monitor). No kernel uevent source
    /// exists yet, so the fd accepts binds/sends but never becomes readable.
    Netlink { nonblock: bool },
    /// `inotify` instance (deniald's settings watcher). No filesystem events
    /// are published, so reads block/EAGAIN forever.
    Inotify { nonblock: bool },
    /// The `/dev/udmabuf` controller fd: `UDMABUF_CREATE` turns a memfd into
    /// a dmabuf fd (served as a second MemFd target over the same state).
    Udmabuf,
}

struct SignalFdState {
    mask: u64,
    nonblock: bool,
    queued: Vec<u32>,
}

/// Capsicum-style rights for Linux descriptors. The common bits match the
/// native VFS masks; the upper bits cover operations only the Linux ABI
/// exposes.
pub const FD_READ: u32 = vfs::FD_READ;
pub const FD_WRITE: u32 = vfs::FD_WRITE;
pub const FD_SEEK: u32 = vfs::FD_SEEK;
pub const FD_FSTAT: u32 = vfs::FD_FSTAT;
pub const FD_READDIR: u32 = vfs::FD_READDIR;
pub const FD_LOOKUP: u32 = vfs::FD_LOOKUP;
pub const FD_TRUNCATE: u32 = vfs::FD_TRUNCATE;
pub const FD_IOCTL: u32 = 1 << 7;
pub const FD_MMAP: u32 = 1 << 8;
pub const FD_POLL: u32 = 1 << 9;
pub const FD_EVENT: u32 = 1 << 10;
pub const FD_ACCEPT: u32 = 1 << 11;
pub const FD_CONNECT: u32 = 1 << 12;
pub const FD_BIND: u32 = 1 << 13;
pub const FD_LISTEN: u32 = 1 << 14;
pub const FD_SEND: u32 = 1 << 15;
pub const FD_RECV: u32 = 1 << 16;
pub const FD_SHUTDOWN: u32 = 1 << 17;
pub const FD_ALL: u32 = FD_READ
    | FD_WRITE
    | FD_SEEK
    | FD_FSTAT
    | FD_READDIR
    | FD_LOOKUP
    | FD_TRUNCATE
    | FD_IOCTL
    | FD_MMAP
    | FD_POLL
    | FD_EVENT
    | FD_ACCEPT
    | FD_CONNECT
    | FD_BIND
    | FD_LISTEN
    | FD_SEND
    | FD_RECV
    | FD_SHUTDOWN;

struct Table {
    task: u64,
    slots: Vec<Option<Target>>,
    /// `O_ACCMODE` bits of the `open` that created each slot. `fdopen` and
    /// friends cross-check the requested mode against `fcntl(F_GETFL)`, and a
    /// hardcoded `O_RDONLY` made every write stream fail with `EINVAL`.
    access: Vec<u64>,
    /// Capsicum-style rights for each slot. Only ever cleared after insert.
    rights: Vec<u32>,
    /// Per-descriptor ioctl allowlists; `None` means every command the
    /// driver accepts. A `Some` list can only shrink.
    ioctls: Vec<Option<Vec<u32>>>,
    /// Descriptor numbers carrying `FD_CLOEXEC`. `execve` closes exactly
    /// these, which is how Rust's `Command` learns that `execvp` succeeded
    /// (the parent reads EOF from the exec-error socketpair).
    cloexec: Vec<u64>,
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
            tables.push(Table {
                task,
                slots,
                access: alloc::vec![O_RDWR; 3],
                rights: alloc::vec![FD_ALL; 3],
                ioctls: alloc::vec![None; 3],
                cloexec: Vec::new(),
            });
            tables.len() - 1
        }
    };
    action(&mut tables[index])
}

/// POSIX shared-memory names (`shm_open`). glibc implements `shm_open` as
/// `open("/dev/shm/<name>")`, and wlroots 0.20 allocates its keymap and
/// swapchain buffers this way. Names map to a shared memfd state so two
/// opens of the same name see the same bytes until `shm_unlink`.
static SHM_FILES: Mutex<Vec<(alloc::string::String, Arc<Mutex<MemFdState>>)>> =
    Mutex::new(Vec::new());

/// Creates or opens one POSIX shm object and installs it in the fd table.
pub fn shm_open(
    task: u64,
    name: &str,
    create: bool,
    exclusive: bool,
    truncate: bool,
) -> Result<u64, u64> {
    // POSIX shm names are a global namespace, like pathname sockets.
    if scheduler::current_task_capability_mode() {
        return Err(EPERM);
    }
    let state = {
        let mut files = SHM_FILES.lock();
        let existing = files
            .iter()
            .find(|(stored, _)| stored == name)
            .map(|(_, state)| state.clone());
        match existing {
            Some(state) => {
                if create && exclusive {
                    return Err(EEXIST);
                }
                if truncate {
                    let mut file = state.lock();
                    file.data.clear();
                    file.len = 0;
                    file.offset = 0;
                }
                state
            }
            None => {
                if !create {
                    return Err(ENOENT);
                }
                let state = Arc::new(Mutex::new(MemFdState {
                    data: Vec::new(),
                    len: 0,
                    offset: 0,
                    frames: None,
                    maps: 0,
                    seals: 0,
                }));
                files.push((name.into(), state.clone()));
                state
            }
        }
    };
    insert(task, Target::MemFd(state)).ok_or(EMFILE)
}

/// `shm_unlink`: forget a name. Open descriptors keep the object alive.
pub fn shm_unlink(name: &str) -> Result<(), u64> {
    if scheduler::current_task_capability_mode() {
        return Err(EPERM);
    }
    let mut files = SHM_FILES.lock();
    let Some(index) = files.iter().position(|(stored, _)| stored == name) else {
        return Err(ENOENT);
    };
    files.remove(index);
    Ok(())
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
        for (target, _, _) in message.fds {
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
/// `O_ACCMODE` value shared with the Linux ABI (read is `0`).
const O_RDWR: u64 = 2;

fn set_access(table: &mut Table, fd: u64, access: u64) {
    while table.access.len() <= fd as usize {
        table.access.push(O_RDWR);
    }
    table.access[fd as usize] = access;
}

fn access_at(table: &Table, fd: u64) -> u64 {
    table.access.get(fd as usize).copied().unwrap_or(O_RDWR)
}

fn set_rights(table: &mut Table, fd: u64, rights: u32) {
    while table.rights.len() <= fd as usize {
        table.rights.push(FD_ALL);
    }
    table.rights[fd as usize] = rights;
}

fn rights_at(table: &Table, fd: u64) -> u32 {
    table.rights.get(fd as usize).copied().unwrap_or(FD_ALL)
}

fn set_ioctls(table: &mut Table, fd: u64, list: Option<Vec<u32>>) {
    while table.ioctls.len() <= fd as usize {
        table.ioctls.push(None);
    }
    table.ioctls[fd as usize] = list;
}

fn ioctls_at(table: &Table, fd: u64) -> Option<Vec<u32>> {
    table.ioctls.get(fd as usize).cloned().unwrap_or(None)
}

/// Rights granted by opening a target. The access mode narrows these in
/// `insert_access`; targets installed by `socket`, `pipe` or `memfd` get the
/// full set their kind can use.
fn default_rights(target: &Target) -> u32 {
    match target {
        Target::Vfs(_) => {
            FD_READ
                | FD_WRITE
                | FD_SEEK
                | FD_FSTAT
                | FD_READDIR
                | FD_LOOKUP
                | FD_TRUNCATE
                | FD_IOCTL
                | FD_MMAP
                | FD_POLL
        }
        Target::DirList { .. } | Target::SysfsDir { .. } => {
            FD_READDIR | FD_LOOKUP | FD_FSTAT | FD_SEEK | FD_POLL
        }
        Target::Sysfs(..) | Target::Proc(..) => {
            FD_READ | FD_SEEK | FD_FSTAT | FD_IOCTL | FD_POLL
        }
        Target::SysfsLink(_) => FD_LOOKUP | FD_FSTAT | FD_POLL,
        Target::Drm { .. } | Target::Prime { .. } | Target::GpuFb | Target::Ivshmem => {
            FD_READ | FD_WRITE | FD_FSTAT | FD_IOCTL | FD_MMAP | FD_POLL
        }
        Target::Socket { .. } | Target::Unbound { .. } | Target::Listener { .. } => {
            FD_READ
                | FD_WRITE
                | FD_FSTAT
                | FD_IOCTL
                | FD_POLL
                | FD_ACCEPT
                | FD_CONNECT
                | FD_BIND
                | FD_LISTEN
                | FD_SEND
                | FD_RECV
                | FD_SHUTDOWN
        }
        Target::Console => FD_READ | FD_WRITE | FD_FSTAT | FD_IOCTL | FD_POLL,
        Target::MemFd(_) => {
            FD_READ
                | FD_WRITE
                | FD_SEEK
                | FD_FSTAT
                | FD_TRUNCATE
                | FD_IOCTL
                | FD_MMAP
                | FD_POLL
        }
        Target::Pipe { .. }
        | Target::EventFd { .. }
        | Target::SignalFd(_)
        | Target::TimerFd { .. }
        | Target::Netlink { .. }
        | Target::Inotify { .. } => FD_READ | FD_WRITE | FD_FSTAT | FD_IOCTL | FD_POLL,
        Target::Epoll(_) => FD_READ | FD_WRITE | FD_FSTAT | FD_IOCTL | FD_POLL | FD_EVENT,
        _ => FD_READ | FD_WRITE | FD_FSTAT | FD_IOCTL | FD_POLL,
    }
}

fn narrow_for_access(table: &mut Table, fd: u64, access: u64) {
    let mut rights = rights_at(table, fd);
    match access & 3 {
        0 => rights &= !(FD_WRITE | FD_TRUNCATE),
        1 => rights &= !FD_READ,
        _ => {}
    }
    set_rights(table, fd, rights);
}

fn insert_in(table: &mut Table, target: Target) -> Option<u64> {
    let rights = default_rights(&target);
    for (index, slot) in table.slots.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(target);
            set_access(table, index as u64, O_RDWR);
            set_rights(table, index as u64, rights);
            set_ioctls(table, index as u64, None);
            return Some(index as u64);
        }
    }
    if table.slots.len() >= MAX_FDS {
        return None;
    }
    table.slots.push(Some(target));
    set_access(table, (table.slots.len() - 1) as u64, O_RDWR);
    set_rights(table, (table.slots.len() - 1) as u64, rights);
    set_ioctls(table, (table.slots.len() - 1) as u64, None);
    Some((table.slots.len() - 1) as u64)
}

fn insert(task: u64, target: Target) -> Option<u64> {
    with_table(task, |table| insert_in(table, target))
}

/// Like [`insert`], but records the descriptor's `O_ACCMODE` bits so
/// `fcntl(F_GETFL)` answers what `open` was asked for.
fn insert_access(task: u64, target: Target, access: u64) -> Option<u64> {
    with_table(task, |table| {
        let fd = insert_in(table, target)?;
        set_access(table, fd, access);
        narrow_for_access(table, fd, access);
        Some(fd)
    })
}

fn insert_from_in(table: &mut Table, target: Target, minimum: u64) -> Option<u64> {
    let rights = default_rights(&target);
    let mut index = minimum as usize;
    while index < table.slots.len() {
        if table.slots[index].is_none() {
            table.slots[index] = Some(target);
            set_access(table, index as u64, O_RDWR);
            set_rights(table, index as u64, rights);
            set_ioctls(table, index as u64, None);
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
    set_access(table, (table.slots.len() - 1) as u64, O_RDWR);
    set_rights(table, (table.slots.len() - 1) as u64, rights);
    set_ioctls(table, (table.slots.len() - 1) as u64, None);
    Some((table.slots.len() - 1) as u64)
}

/// Current rights of `fd`; `EBADF` when the descriptor is not open.
pub fn rights(task: u64, fd: u64) -> Result<u32, u64> {
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        Ok(rights_at(table, fd))
    })
}

/// Clears every right not in `mask`. Widening is impossible by construction;
/// unknown bits are rejected so a future right cannot be silently ignored.
pub fn limit_rights(task: u64, fd: u64, mask: u32) -> Result<u32, u64> {
    if mask & !FD_ALL != 0 {
        return Err(EINVAL);
    }
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        let rights = rights_at(table, fd) & mask;
        set_rights(table, fd, rights);
        Ok(rights)
    })
}

/// Maximum entries in one per-descriptor ioctl allowlist.
const MAX_IOCTLS: usize = 256;

/// Replaces `fd`'s ioctl allowlist. The new list must be a subset of the
/// current one, so a capability can only narrow; `None` is the initial
/// unrestricted state.
pub fn limit_ioctls(task: u64, fd: u64, commands: &[u32]) -> Result<(), u64> {
    if commands.len() > MAX_IOCTLS {
        return Err(EINVAL);
    }
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        if let Some(current) = table.ioctls.get(fd as usize).and_then(|list| list.as_ref()) {
            if commands.iter().any(|command| !current.contains(command)) {
                return Err(EPERM);
            }
        }
        set_ioctls(table, fd, Some(commands.to_vec()));
        Ok(())
    })
}

/// Requires `FD_IOCTL` and, when the descriptor has an allowlist, one of
/// its commands. The driver still validates the command it receives.
pub fn ioctl_allowed(task: u64, fd: u64, request: u64) -> Result<(), u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_IOCTL)?;
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        match table.ioctls.get(fd as usize).and_then(|list| list.as_ref()) {
            None => Ok(()),
            Some(commands) if commands.contains(&(request as u32)) => Ok(()),
            Some(_) => Err(EPERM),
        }
    })
}

/// Fails with `EPERM` when `need` is not a subset of the descriptor's rights.
pub fn require_right(task: u64, fd: u64, need: u32) -> Result<(), u64> {
    let rights = rights(task, fd)?;
    if rights & need == need {
        Ok(())
    } else {
        Err(EPERM)
    }
}

/// Nodes listed by the synthetic `/dev/dri` directory.
const DRM_NODES: &[&str] = &["card0", "renderD128"];
/// `/dev` listing: udev enumerate and glib scan the device root.
const DEV_NODES: &[&str] = &[
    "dri", "input", "shm", "null", "zero", "full", "urandom", "random", "console", "tty0", "tty1",
];

/// The `/dev/input` directory, sized to the devices the kernel brought up.
fn input_nodes() -> &'static [&'static str] {
    match crate::virtio_input::count() {
        0 => &[],
        1 => &["event0"],
        _ => &["event0", "event1"],
    }
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

    let resolved = match crate::linux_abi::resolve(cwd, path) {
        Ok(resolved) => resolved,
        Err(()) => {
            return Err(ENOENT);
        }
    };
    // /proc/self/fd/N is a magic link. systemd's fd_reopen() opens it to
    // duplicate an O_PATH directory descriptor; path-based reopening covers
    // the VFS and sysfs targets that the udev device walk uses.
    if let Some(rest) = resolved.strip_prefix("/proc/self/fd/") {
        if let Ok(number) = rest.parse::<u64>() {
            let reopen = match lookup(task, number) {
                Some(Target::Vfs(vfs_fd)) => vfs::fd_path(task, vfs_fd)
                    .map(|path| crate::linux_abi::fs::display_path(&path)),
                Some(Target::SysfsDir { path, .. }) => Some(path),
                _ => None,
            };
            if let Some(path) = reopen {
                return open(task, &path, ".", flags);
            }
            return Err(ENOENT);
        }
    }
    // Per-fd fdinfo: systemd's fds_are_same_mount() reads
    // /proc/<pid>/fdinfo/<fd> for the "mnt_id" line and asserts on failure.
    // The content is dynamic (per fd), so it is served through a memfd.
    // Only the caller's own thread group is served; another pid's fdinfo is
    // ENOENT like on a real kernel without permission.
    if resolved.starts_with("/proc/") {
        if let Some((pid_str, fd_str)) = resolved[5..].split_once("/fdinfo/") {
            let own = match pid_str {
                "self" => true,
                p => p.parse::<u64>().ok().map(|pid| {
                    pid == crate::scheduler::current_tgid()
                        || pid == crate::scheduler::current_task_id()
                }).unwrap_or(false),
            };
            if own {
                let number = fd_str.parse::<u64>().map_err(|_| ENOENT)?;
                let bytes = fdinfo_content(task, number).ok_or(ENOENT)?;
                return insert(
                    task,
                    Target::MemFd(Arc::new(Mutex::new(MemFdState {
                        data: bytes.clone(),
                        len: bytes.len(),
                        offset: 0,
                        frames: None,
                        maps: 0,
                        seals: 0,
                    }))),
                )
                .ok_or(EMFILE);
            }
        }
    }
    // Synthetic procfs first: OpenRC reads a handful of /proc files.
    if let Some(node) = crate::procfs::lookup(&resolved) {
        return match node {
            crate::procfs::Node::File(data) => {
                insert_access(task, Target::Proc(data, 0), 0).ok_or(EMFILE)
            }
            crate::procfs::Node::Dir(entries) => {
                insert_access(task, Target::DirList { entries, offset: 0 }, 0).ok_or(EMFILE)
            }
            crate::procfs::Node::Symlink(target) => {
                match target {
                    "/dev/console" => insert(task, Target::Console).ok_or(EMFILE),
                    _ => Err(EINVAL),
                }
            }
        };
    }
    // Synthetic sysfs next: it exists only in the kernel.
    if let Some(node) = crate::sysfs::lookup(&resolved) {
        return match node {
            crate::sysfs::Node::File(data) => {
                insert_access(task, Target::Sysfs(data, 0), 0).ok_or(EMFILE)
            }
            crate::sysfs::Node::Dir => insert_access(
                task,
                Target::SysfsDir {
                    entries: crate::sysfs::entries(&resolved),
                    path: resolved.clone(),
                    offset: 0,
                },
                0,
            )
            .ok_or(EMFILE),
            crate::sysfs::Node::Symlink(target) => {
                // openat(..., O_PATH|O_NOFOLLOW) must yield the link itself;
                // a plain open follows it like the kernel would.
                const O_NOFOLLOW: u64 = 0o100000;
                if flags & O_NOFOLLOW != 0 {
                    insert_access(task, Target::SysfsLink(target), 0).ok_or(EMFILE)
                } else {
                    open(task, target, ".", flags)
                }
            }
        };
    }
    // Anything else under the synthetic /proc and /sys trees does not exist:
    // probing libraries expect the clean ENOENT instead of the VFS EACCES.
    if resolved == "/proc" || resolved.starts_with("/proc/") || resolved.starts_with("/sys/") {
        return Err(ENOENT);
    }
    if let Some(name) = resolved.strip_prefix("/dev/shm/") {
        if name.is_empty() || name.contains('/') {
            return Err(ENOENT);
        }
        return shm_open(
            task,
            name,
            flags & O_CREAT != 0,
            flags & O_EXCL != 0,
            flags & O_TRUNC != 0,
        );
    }
    if resolved == "/dev/shm" {
        if flags & O_DIRECTORY == 0 {
            return Err(EISDIR);
        }
        return insert(
            task,
            Target::DirList {
                entries: &[],
                offset: 0,
            },
        )
        .ok_or(EMFILE);
    }
    // `/dev/tty` plus the VT range (`/dev/tty0`..`/dev/tty63`): seatd opens
    // the per-session VT path, so every numeric tty is the same console.
    let tty_console = resolved
        .strip_prefix("/dev/tty")
        .map(|rest| rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false);
    // libdrm resolves devices through the PCI aliases under /dev/dri/by-path
    // (`pci-0000:0a:00.0-card` / `-render`); they are the same nodes.
    let dri_alias = resolved
        .strip_prefix("/dev/dri/by-path/")
        .filter(|_| crate::virtio_gpu::size().is_some())
        .and_then(|name| {
            if name.ends_with("-card") {
                Some(Target::Drm { render: false })
            } else if name.ends_with("-render") {
                Some(Target::Drm { render: true })
            } else {
                None
            }
        });
    let device = if tty_console {
        Some(Target::Console)
    } else if let Some(node) = dri_alias {
        Some(node)
    } else {
        match resolved.as_str() {
        "/dev/null" => Some(Target::Null),
        "/dev/zero" => Some(Target::Zero),
        "/dev/full" => Some(Target::Full),
        "/dev/urandom" | "/dev/random" => Some(Target::Random),
        "/dev/ivshmem" => Some(Target::Ivshmem),
        // The udmabuf controller: llvmpipe opens it to turn memfds into
        // dmabuf fds, which is how it builds its native fence support.
        "/dev/udmabuf" => Some(Target::Udmabuf),
        // Only present when the machine has a virtio-gpu the driver brought
        // up; the compositor then presents through the GPU as well.
        "/dev/gpufb" if crate::virtio_gpu::size().is_some() => Some(Target::GpuFb),
        "/dev" => Some(Target::DirList {
            entries: DEV_NODES,
            offset: 0,
        }),
        "/dev/input" if crate::virtio_input::present() => Some(Target::DirList {
            entries: input_nodes(),
            offset: 0,
        }),
        "/dev/input/event0" if crate::virtio_input::present() => Some(Target::Input(0)),
        "/dev/input/event1" if crate::virtio_input::count() > 1 => Some(Target::Input(1)),
        "/dev/dri" if crate::virtio_gpu::size().is_some() => Some(Target::DirList {
            entries: DRM_NODES,
            offset: 0,
        }),
            "/dev/dri/card0" if crate::virtio_gpu::size().is_some() => {
                Some(Target::Drm { render: false })
            }
            "/dev/dri/renderD128" if crate::virtio_gpu::size().is_some() => {
                Some(Target::Drm { render: true })
            }
            _ => None,
        }
    };
    if let Some(device) = device {
        return insert(task, device).ok_or(EMFILE);
    }
    // `/` is the guest root (the SFS mount internally); every Linux task may
    // reopen it even though the policy prefix is `/sfs`.
    if resolved != "/" && !crate::scheduler::read_path_allowed(task, &resolved) {
        return Err(EACCES);
    }
    let wants_write =
        flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND) != 0;
    if wants_write {
        // SFS is read-only except for the service subtrees an init writes.
        if !crate::scheduler::linux_write_allowed(&resolved) {
            return Err(EROFS);
        }
        let exists = vfs::file_len("/", &resolved).is_ok();
        if flags & O_CREAT != 0 && flags & O_EXCL != 0 && exists {
            return Err(EEXIST);
        }
        if flags & O_TRUNC != 0 {
            vfs::create("/", &resolved).map_err(|_| EACCES)?;
        } else if !exists {
            if flags & O_CREAT == 0 {
                return Err(ENOENT);
            }
            vfs::create("/", &resolved).map_err(|_| EACCES)?;
        }
        let vfs_flags = crate::vfs::OPEN_WRITE
            | if flags & O_APPEND != 0 {
                crate::vfs::OPEN_APPEND
            } else {
                0
            };
        let fd = vfs::open(task, "/", &resolved, vfs_flags).map_err(|_| EACCES)?;
        return insert_access(task, Target::Vfs(fd), flags & 0o3).ok_or(EMFILE);
    }
    if flags & O_DIRECTORY != 0 {
        let fd = vfs::open_dir(task, "/", &resolved).map_err(|_| ENOENT)?;
        return insert_access(task, Target::Vfs(fd), flags & 0o3).ok_or(EMFILE);
    }
    // Linux allows opening a directory without O_DIRECTORY.
    let vfs_fd = match vfs::open(task, "/", &resolved, 0) {
        Ok(fd) => fd,
        Err(_) => match vfs::open_dir(task, "/", &resolved) {
            Ok(fd) => fd,
            Err(()) => {
                return Err(ENOENT);
            }
        },
    };
    match insert_access(task, Target::Vfs(vfs_fd), flags & 0o3) {
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
        let slot = table.slots.get_mut(fd as usize)?.take();
        if slot.is_some() {
            table.cloexec.retain(|&marked| marked != fd);
            set_ioctls(table, fd, None);
        }
        slot
    })
    .ok_or(EBADF)?;
    // Temporary: socket teardown forensics — pairs a client EOF with the
    // closer's task and fd.
    if matches!(target, Target::Socket { .. }) {
    }
    target.release_endpoint();
    if let Target::Vfs(vfs_fd) = target {
        close_vfs_if_last(task, vfs_fd);
    }
    Ok(())
}

/// `dup`/`dup2` share one VFS descriptor (and its offset), so closing one
/// Linux slot must not retire it while another slot still names it. libxml2
/// dups an fd, closes the original and reads from the duplicate; without this
/// the read returned EBADF and xkbregistry could not load its rules.
fn close_vfs_if_last(task: u64, vfs_fd: u64) {
    let still_open = with_table(task, |table| {
        table.slots.iter().flatten().any(|other| {
            matches!(other, Target::Vfs(other_fd) if *other_fd == vfs_fd)
        })
    });
    if !still_open {
        let _ = vfs::close(task, vfs_fd);
    }
}

pub fn dup(task: u64, fd: u64) -> Result<u64, u64> {
    with_table(task, |table| {
        let target = table
            .slots
            .get(fd as usize)
            .and_then(|slot| slot.clone())
            .ok_or(EBADF)?;
        let access = access_at(table, fd);
        let rights = rights_at(table, fd);
        let ioctls = ioctls_at(table, fd);
        let new_fd = insert_in(table, target.clone()).ok_or(EMFILE)?;
        set_access(table, new_fd, access);
        set_rights(table, new_fd, rights);
        set_ioctls(table, new_fd, ioctls);
        target.retain_endpoint();
        Ok(new_fd)
    })
}

pub fn dup_from(task: u64, fd: u64, minimum: u64, cloexec: bool) -> Result<u64, u64> {
    let new_fd = with_table(task, |table| {
        let target = table
            .slots
            .get(fd as usize)
            .and_then(|slot| slot.clone())
            .ok_or(EBADF)?;
        let access = access_at(table, fd);
        let rights = rights_at(table, fd);
        let ioctls = ioctls_at(table, fd);
        let new_fd = insert_from_in(table, target.clone(), minimum).ok_or(EMFILE)?;
        set_access(table, new_fd, access);
        set_rights(table, new_fd, rights);
        set_ioctls(table, new_fd, ioctls);
        target.retain_endpoint();
        Ok::<u64, u64>(new_fd)
    })?;
    if cloexec {
        let _ = set_cloexec(task, new_fd, true);
    }
    Ok(new_fd)
}

/// `fcntl(F_GETFD)`: reports `FD_CLOEXEC`.
pub fn get_cloexec(task: u64, fd: u64) -> Result<bool, u64> {
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        Ok(table.cloexec.contains(&fd))
    })
}

/// `fcntl(F_SETFD)`.
pub fn set_cloexec(task: u64, fd: u64, enabled: bool) -> Result<(), u64> {
    with_table(task, |table| {
        if table.slots.get(fd as usize).and_then(|slot| slot.as_ref()).is_none() {
            return Err(EBADF);
        }
        table.cloexec.retain(|&marked| marked != fd);
        if enabled {
            table.cloexec.push(fd);
        }
        Ok(())
    })
}

/// Closes every descriptor marked `FD_CLOEXEC`. Called after a successful
/// `execve`, exactly like the kernel does on exec.
pub fn close_cloexec(task: u64) {
    let task = crate::scheduler::fd_table_owner(task);
    let targets: Vec<Target> = with_table(task, |table| {
        let mut closed = Vec::new();
        for fd in core::mem::take(&mut table.cloexec) {
            if let Some(slot) = table.slots.get_mut(fd as usize) {
                if let Some(target) = slot.take() {
                    closed.push(target);
                }
            }
        }
        closed
    });
    for target in targets {
        target.release_endpoint();
        if let Target::Vfs(vfs_fd) = target {
            let _ = vfs::close(task, vfs_fd);
        }
    }
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
            close_vfs_if_last(task, vfs_fd);
        }
    }
    let (access, rights, ioctls) = with_table(task, |table| {
        (access_at(table, oldfd), rights_at(table, oldfd), ioctls_at(table, oldfd))
    });
    target.retain_endpoint();
    with_table(task, |table| {
        table.slots[newfd as usize] = Some(target);
        set_access(table, newfd, access);
        set_rights(table, newfd, rights);
        set_ioctls(table, newfd, ioctls);
        table.cloexec.retain(|&marked| marked != newfd);
    });
    Ok(newfd)
}

pub fn read(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_READ)?;
    if count == 0 {
        return Ok(0);
    }
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        // stdin is not wired to the console yet; report EOF so a reader does
        // not spin.
        Target::Console => Ok(0),
        Target::DirList { .. } | Target::SysfsDir { .. } => Err(EISDIR),
        Target::SysfsLink(_) => Err(EINVAL),
        Target::Prime { .. } => Err(EINVAL),
        // The controller fd carries no data; the dmabuf form is a MemFd.
        Target::Udmabuf => Err(EINVAL),
        Target::Sysfs(data, offset) => {
            if offset >= data.len() {
                return Ok(0);
            }
            let take = (count as usize).min(data.len() - offset);
            user_memory::copy_to_user(&data[offset..offset + take], buffer)
                .map_err(|_| EFAULT)?;
            // Advance the descriptor's offset in place.
            let _ = with_table(task, |table| {
                if let Some(Target::Sysfs(_, position)) =
                    table.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
                {
                    *position += take;
                }
            });
            Ok(take as u64)
        }
        Target::Proc(data, offset) => {
            if offset >= data.len() {
                return Ok(0);
            }
            let take = (count as usize).min(data.len() - offset);
            user_memory::copy_to_user(&data[offset..offset + take], buffer)
                .map_err(|_| EFAULT)?;
            let _ = with_table(task, |table| {
                if let Some(Target::Proc(_, position)) =
                    table.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
                {
                    *position += take;
                }
            });
            Ok(take as u64)
        }
        Target::Drm { .. } => match crate::drm::read_event() {
            Some(event) => {
                user_memory::copy_to_user(&event, buffer).map_err(|_| EFAULT)?;
                Ok(event.len() as u64)
            }
            // The caller polls before reading; an empty queue is not an error.
            None => Ok(0),
        },
        Target::Input(index) => {
            crate::virtio_input::pump();
            let length = (count as usize).min(64 * 1024);
            let mut records = alloc::vec![0_u8; length];
            let filled = crate::virtio_input::read_events(index, &mut records);
            if filled == 0 {
                // libinput opens evdev non-blocking and relies on EAGAIN to
                // stop draining; zero would look like end-of-device.
                return Err(EAGAIN);
            }
            user_memory::copy_to_user(&records[..filled], buffer).map_err(|_| EFAULT)?;
            Ok(filled as u64)
        }
        Target::Null | Target::Full | Target::Ivshmem | Target::GpuFb => Ok(0),
        Target::Zero => {
            let chunk = alloc::vec![0_u8; (count as usize).min(64 * 1024)];
            user_memory::copy_to_user(&chunk, buffer).map_err(|_| EFAULT)?;
            Ok(chunk.len() as u64)
        }
        Target::Random => {
            let length = (count as usize).min(64 * 1024);
            let mut bytes = alloc::vec![0_u8; length];
            crate::entropy::fill(&mut bytes);
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
        Target::SignalFd(state) => signalfd_read(&state, buffer, count),
        Target::MemFd(state) => memfd_read(&state, buffer, count),
        Target::Socket { state, end, nonblock } => {
            let mut chunk = alloc::vec![0_u8; (count as usize).min(64 * 1024)];
            let (read, fds) = socket_recv_message(&state, end, nonblock, &mut chunk)?;
            for (target, rights, ioctls) in fds {
                let _ = insert_transferred(task, target, rights, ioctls);
            }
            user_memory::copy_to_user(&chunk[..read], buffer).map_err(|_| EFAULT)?;
            Ok(read as u64)
        }
        Target::Unbound { .. } | Target::Listener { .. } => Err(ENOTCONN),
        // The netlink stub never carries uevent data.
        Target::Netlink { .. } => Err(EAGAIN),
        // No filesystem events are published either.
        Target::Inotify { .. } => Err(EAGAIN),
        Target::TimerFd { state, nonblock } => timerfd_read(&state, nonblock, buffer),
        Target::Epoll(_) => Err(EINVAL),
    }
}

/// Writes a kernel buffer to a descriptor (`sendfile`, `copy_file_range`).
/// Only regular files and the null/zero devices are supported; a file write
/// keeps the same RamFs quota check as the userspace `write` path.
pub fn write_kernel(task: u64, fd: u64, bytes: &[u8]) -> Result<usize, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_WRITE)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    match target {
        Target::Vfs(vfs_fd) => {
            let blocks = (bytes.len().div_ceil(512) + 2) as u32;
            if !crate::scheduler::fs_quota_allows(blocks, 0) {
                return Err(ENOSPC);
            }
            vfs::write(task, vfs_fd, bytes).map_err(|_| EBADF)
        }
        Target::Null | Target::Zero => Ok(bytes.len()),
        _ => Err(ENODEV),
    }
}

/// Positional read used by file-backed `mmap`. Does not move the fd offset.
pub fn pread(task: u64, fd: u64, offset: u64, buffer: &mut [u8]) -> Result<usize, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_READ | FD_SEEK)?;
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
                let count = count.min(file.data.len().saturating_sub(start));
                buffer[..count].copy_from_slice(&file.data[start..start + count]);
            }
            Ok(count)
        }
        Target::Zero => {
            let count = buffer.len();
            buffer.fill(0);
            Ok(count)
        }
        Target::Input(index) => {
            crate::virtio_input::pump();
            let count = crate::virtio_input::read_events(index, buffer);
            if count == 0 {
                Err(EAGAIN)
            } else {
                Ok(count)
            }
        }
        Target::Null => Ok(0),
        _ => Err(ENODEV),
    }
}

pub fn write(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_WRITE)?;
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
        Target::Input(_)
        | Target::Null
        | Target::Zero
        | Target::Random
        | Target::Ivshmem
        | Target::GpuFb
        | Target::Drm { .. }
        | Target::Sysfs(_, _)
        | Target::Proc(_, _)
        | Target::DirList { .. }
        | Target::SysfsDir { .. }
        | Target::SysfsLink(_)
        | Target::Prime { .. } => Ok(count),
        Target::Full => Err(ENOSPC),
        Target::Vfs(vfs_fd) => {
            let mut bytes = alloc::vec![0_u8; (count as usize).min(1 << 20)];
            user_memory::copy_from_user(buffer, &mut bytes).map_err(|_| EFAULT)?;
            // The native `SYS_FS_WRITE` path checks the manifest quota before
            // touching the VFS. The Linux path must enforce the same budget,
            // or a task with a small quota could fill the shared RamFs up to
            // the global limit.
            let blocks = (bytes.len().div_ceil(512) + 2) as u32;
            if !crate::scheduler::fs_quota_allows(blocks, 0) {
                return Err(ENOSPC);
            }
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
        // Netlink sends vanish (no kernel uevent publisher reads them).
        Target::Netlink { .. } => Ok(count),
        Target::Inotify { .. } => Err(EINVAL),
        Target::Udmabuf => Err(EINVAL),
        Target::TimerFd { .. } | Target::Epoll(_) | Target::SignalFd(_) => Err(EINVAL),
    }
}

pub fn lseek(task: u64, fd: u64, offset: i64, whence: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_SEEK)?;
    match lookup(task, fd).ok_or(EBADF)? {
        Target::Vfs(vfs_fd) => vfs::lseek(task, vfs_fd, offset, whence).map_err(|_| EINVAL),
        Target::DirList { .. } | Target::SysfsDir { .. } => {
            // `rewinddir` only needs SEEK_SET 0.
            if whence == 0 && offset == 0 {
                let _ = with_table(task, |table| {
                    if let Some(slot) =
                        table.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
                    {
                        match slot {
                            Target::DirList { offset, .. } | Target::SysfsDir { offset, .. } => {
                                *offset = 0;
                            }
                            _ => {}
                        }
                    }
                });
                Ok(0)
            } else {
                Err(EINVAL)
            }
        }
        Target::MemFd(state) => {
            let mut file = state.lock();
            let base = match whence {
                0 => 0,
                1 => file.offset as i64,
                2 => file.len() as i64,
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
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_WRITE | FD_SEEK)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    if let Target::Vfs(vfs_fd) = target {
        return vfs::pwrite(task, vfs_fd, offset, bytes).map_err(|_| EINVAL);
    }
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let mut file = state.lock();
    let start = offset as usize;
    let end = start.checked_add(bytes.len()).ok_or(EINVAL)?;
    if file.frames.is_some() || end > MEMFD_SHADOW_LIMIT {
        if file.frames.is_none() {
            file.ensure_frames(end)?;
        }
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
    if end > file.len {
        file.len = end;
    }
    Ok(bytes.len())
}

/// Stable synthetic inode for a VFS path. The dynamic loader detects "same
/// file mapped twice" by `(st_dev, st_ino)`; returning the descriptor number
/// made two different libraries look identical because the loader reuses fd
/// numbers, so it silently reused the first library's link map.
pub fn path_ino(path: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 { 1 } else { hash }
}

/// Fills the aarch64 `struct stat` for a descriptor.
/// Mode, size, inode and (for device nodes) rdev of one open descriptor.
/// Shared by `fstat` and `statx`, which cannot reuse the `struct stat`
/// writer without a user buffer.
pub fn stat_fields(
    task: u64,
    fd: u64,
) -> Result<(u32, u64, u64, Option<u64>), u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_FSTAT)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    Ok(match target {
        Target::Console => (S_IFCHR | 0o620, 0, fd.max(1), None),
        Target::Null
        | Target::Zero
        | Target::Full
        | Target::Random
        | Target::Ivshmem
        | Target::GpuFb => (S_IFCHR | 0o666, 0, fd.max(1), None),
        Target::Input(index) => (
            S_IFCHR | 0o660,
            0,
            fd.max(1),
            Some(makedev(
                crate::virtio_input::EVDEV_MAJOR,
                crate::virtio_input::EVDEV_MINOR_BASE + index as u32,
            )),
        ),
        Target::Drm { render } => (
            S_IFCHR | 0o666,
            0,
            fd.max(1),
            Some(makedev(
                crate::sysfs::DRM_MAJOR,
                if render {
                    crate::sysfs::RENDER_MINOR
                } else {
                    crate::sysfs::CARD_MINOR
                },
            )),
        ),
        Target::Sysfs(data, _) => (S_IFREG | 0o444, data.len() as u64, fd.max(1), None),
        Target::SysfsLink(target) => (S_IFLNK | 0o777, target.len() as u64, fd.max(1), None),
        Target::Proc(data, _) => (S_IFREG | 0o444, data.len() as u64, fd.max(1), None),
        Target::DirList { .. } | Target::SysfsDir { .. } => {
            (S_IFDIR | 0o555, 0, fd.max(1), None)
        }
        Target::Prime { size, .. } => (S_IFREG | 0o600, size as u64, fd.max(1), None),
        Target::Vfs(vfs_fd) => {
            let size = vfs::fd_len(task, vfs_fd).map_err(|_| EBADF)?;
            let kind = vfs::fd_kind(task, vfs_fd).unwrap_or(vfs::FD_KIND_FILE);
            let path = vfs::fd_path(task, vfs_fd);
            let mode = if kind == vfs::FD_KIND_DIR {
                S_IFDIR | 0o755
            } else if path
                .as_deref()
                .is_some_and(crate::scheduler::linux_ram_path)
            {
                S_IFREG | 0o644
            } else {
                // The SFS image carries no permission bits and is read-only;
                // report its files executable so access(X_OK) and gio's
                // g_find_program_in_path() accept the programs in /sfs.
                S_IFREG | 0o555
            };
            let ino = path.map(|path| path_ino(&path)).unwrap_or(fd.max(1));
            (mode, size as u64, ino, None)
        }
        Target::Pipe { .. } => (S_IFIFO | 0o600, 0, fd.max(1), None),
        Target::EventFd { .. } | Target::SignalFd(_) => (S_IFREG | 0o600, 0, fd.max(1), None),
        Target::MemFd(state) => (S_IFREG | 0o600, state.lock().len() as u64, fd.max(1), None),
        Target::Socket { .. } | Target::Unbound { .. } | Target::Listener { .. } => {
            (S_IFSOCK | 0o600, 0, fd.max(1), None)
        }
        Target::Epoll(_) | Target::TimerFd { .. } | Target::Netlink { .. }
        | Target::Inotify { .. } => (S_IFSOCK | 0o600, 0, fd.max(1), None),
        Target::Udmabuf => (S_IFCHR | 0o600, 0, fd.max(1), None),
    })
}

/// Filesystem magic for `fstatfs`, so systemd's `fd_is_fs_type(fd,
/// SYSFS_MAGIC)` accepts the synthetic sysfs tree instead of rejecting the
/// device as outside of sysfs.
pub fn fs_magic(task: u64, fd: u64) -> Option<i64> {
    Some(match lookup(task, fd)? {
        Target::Sysfs(..) | Target::SysfsDir { .. } | Target::SysfsLink(_) => 0x6265_6572, // SYSFS_MAGIC
        Target::Proc(..) => 0x0000_9fa0,                           // PROC_SUPER_MAGIC
        Target::DirList { .. } => 0x0102_1994,                     // TMPFS_MAGIC
        _ => 0x5346_5331,                                          // SFS
    })
}

pub fn fstat(task: u64, fd: u64, buffer: u64) -> Result<(), u64> {
    let task = crate::scheduler::fd_table_owner(task);
    let (mode, size, ino, rdev) = stat_fields(task, fd)?;
    match rdev {
        Some(rdev) => write_stat_rdev(buffer, mode, size, ino, rdev),
        None => write_stat(buffer, mode, size, ino),
    }
}

/// Target of a descriptor opened on a sysfs symlink with O_NOFOLLOW. This
/// backs readlinkat(fd, ""), which systemd uses to read subsystem links.
pub fn symlink_target(task: u64, fd: u64) -> Option<&'static str> {
    match lookup(task, fd)? {
        Target::SysfsLink(target) => Some(target),
        _ => None,
    }
}

/// Path of an open directory descriptor, for `*at` calls.
pub fn directory_path(task: u64, fd: u64) -> Option<String> {
    require_right(task, fd, FD_LOOKUP).ok()?;
    match lookup(task, fd)? {
        Target::Vfs(vfs_fd) if vfs::fd_kind(task, vfs_fd) == Some(vfs::FD_KIND_DIR) => vfs::fd_path(
            task, vfs_fd,
        )
        // VFS paths carry the /sfs mount prefix; dirfd-relative lookups need
        // the Linux-visible spelling, otherwise openat(root_fd, "sys") would
        // resolve to /sfs/sys instead of /sys.
        .map(|path| crate::linux_abi::fs::display_path(&path)),
        // libudev opens a sysfs device directory and then resolves "uevent",
        // "subsystem" and "device" relative to that descriptor.
        Target::SysfsDir { path, .. } => Some(path),
        _ => None,
    }
}

pub fn getdents64(task: u64, fd: u64, buffer: u64, count: u64) -> Result<u64, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_READDIR)?;
    const HEADER: usize = 19; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)
    let target = lookup(task, fd).ok_or(EBADF)?;
    if let Target::DirList { entries, offset } = target {
        if count == 0 {
            return Ok(0);
        }
        let limit = (count as usize).min(64 * 1024);
        let mut output = alloc::vec![0_u8; limit];
        let mut used = 0usize;
        let mut serialized = 0usize;
        for (index, name) in entries.iter().enumerate().skip(offset) {
            let name_len = name.len();
            let record = (HEADER + name_len + 1 + 7) & !7;
            if used + record > limit {
                break;
            }
            let ino = (index + 1) as u64;
            output[used..used + 8].copy_from_slice(&ino.to_le_bytes());
            output[used + 8..used + 16].copy_from_slice(&(ino as i64).to_le_bytes());
            output[used + 16..used + 18].copy_from_slice(&(record as u16).to_le_bytes());
            output[used + 18] = 2; // DT_CHR
            output[used + HEADER..used + HEADER + name_len]
                .copy_from_slice(name.as_bytes());
            used += record;
            serialized += 1;
        }
        if serialized == 0 {
            return Ok(0);
        }
        user_memory::copy_to_user(&output[..used], buffer).map_err(|_| EFAULT)?;
        let _ = with_table(task, |table| {
            if let Some(Target::DirList { offset, .. }) =
                table.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
            {
                *offset += serialized;
            }
        });
        return Ok(used as u64);
    }
    if let Target::SysfsDir { entries, offset, .. } = target {
        if count == 0 {
            return Ok(0);
        }
        let limit = (count as usize).min(64 * 1024);
        let mut output = alloc::vec![0_u8; limit];
        let mut used = 0usize;
        let mut serialized = 0usize;
        for (index, entry) in entries.iter().enumerate().skip(offset) {
            let name_len = entry.name.len();
            let record = (HEADER + name_len + 1 + 7) & !7;
            if used + record > limit {
                break;
            }
            let ino = (index + 1) as u64;
            output[used..used + 8].copy_from_slice(&ino.to_le_bytes());
            output[used + 8..used + 16].copy_from_slice(&(ino as i64).to_le_bytes());
            output[used + 16..used + 18].copy_from_slice(&(record as u16).to_le_bytes());
            output[used + 18] = entry.kind;
            output[used + HEADER..used + HEADER + name_len]
                .copy_from_slice(entry.name.as_bytes());
            used += record;
            serialized += 1;
        }
        if serialized == 0 {
            return Ok(0);
        }
        user_memory::copy_to_user(&output[..used], buffer).map_err(|_| EFAULT)?;
        let _ = with_table(task, |table| {
            if let Some(Target::SysfsDir { offset, .. }) =
                table.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
            {
                *offset += serialized;
            }
        });
        return Ok(used as u64);
    }
    let Target::Vfs(vfs_fd) = target else {
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
        output[used + 18] = match entry.kind {
            vfs::FD_KIND_DIR => 4,  // DT_DIR
            3 => 10,                // DT_LNK
            _ => 8,                 // DT_REG
        };
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

/// `signalfd4(2)`: `fd = -1` creates the descriptor, otherwise it updates the
/// mask of an existing signalfd.
pub fn signalfd(task: u64, fd: i64, mask: u64, nonblock: bool) -> Result<u64, u64> {
    if fd == -1 {
        return insert(
            task,
            Target::SignalFd(Arc::new(Mutex::new(SignalFdState {
                mask,
                nonblock,
                queued: Vec::new(),
            }))),
        )
        .ok_or(EMFILE);
    }
    let target = lookup(task, fd as u64).ok_or(EBADF)?;
    let Target::SignalFd(state) = target else {
        return Err(EINVAL);
    };
    state.lock().mask = mask;
    Ok(fd as u64)
}

/// Routes one signal into a matching signalfd of `task` instead of the
/// pending bit. Called from `linux_signal::raise` before the signal table is
/// touched, so the two table locks never nest.
pub fn signalfd_deliver(task: u64, signal: u32) -> bool {
    let bit = match signal {
        0 => return false,
        n if n <= 64 => 1u64 << (n - 1),
        _ => return false,
    };
    let task = crate::scheduler::fd_table_owner(task);
    let mut tables = TABLES.lock();
    let Some(table) = tables.iter_mut().find(|table| table.task == task) else {
        return false;
    };
    for slot in table.slots.iter().flatten() {
        if let Target::SignalFd(state) = slot {
            let mut state = state.lock();
            if state.mask & bit != 0 {
                state.queued.push(signal);
                return true;
            }
        }
    }
    false
}

/// Drains queued signals into `signalfd_siginfo` records (128 bytes each).
fn signalfd_read(
    state: &Arc<Mutex<SignalFdState>>,
    buffer: u64,
    count: u64,
) -> Result<u64, u64> {
    const SIGINFO_SIZE: usize = 128;
    let mut state = state.lock();
    if state.queued.is_empty() {
        // Callers poll first; an empty read then means the queue was drained
        // concurrently, which nonblocking readers treat as EAGAIN.
        return Err(EAGAIN);
    }
    let mut written = 0_u64;
    while !state.queued.is_empty() && written + SIGINFO_SIZE as u64 <= count {
        let signal = state.queued.remove(0);
        let mut record = [0_u8; SIGINFO_SIZE];
        record[0..4].copy_from_slice(&signal.to_le_bytes()); // ssi_signo
        user_memory::copy_to_user(&record, buffer + written).map_err(|_| EFAULT)?;
        written += SIGINFO_SIZE as u64;
    }
    Ok(written)
}

/// Creates an AF_NETLINK socket stub for libudev's uevent monitor.
pub fn netlink_socket(task: u64, nonblock: bool) -> Result<u64, u64> {
    insert(task, Target::Netlink { nonblock }).ok_or(EMFILE)
}

/// Whether the descriptor is the netlink stub (bind/send path checks).
pub fn is_netlink(task: u64, fd: u64) -> bool {
    matches!(
        lookup(task, fd),
        Some(Target::Netlink { .. })
    )
}

/// Creates an inotify instance. Watch descriptors are handed out by the ABI
/// layer; no events are ever queued.
pub fn inotify_init(task: u64, nonblock: bool) -> Result<u64, u64> {
    insert(task, Target::Inotify { nonblock }).ok_or(EMFILE)
}

/// Current eventfd counter without consuming it. DRM syncobj export hands
/// Mesa an eventfd-backed fence fd and needs to know whether it is signalled.
pub fn eventfd_peek(task: u64, fd: u64) -> Option<u64> {
    let target = lookup(task, fd)?;
    match target {
        Target::EventFd { state, .. } => Some(*state.lock()),
        _ => None,
    }
}

/// Adds `value` to an eventfd's counter (the syncobj signal path).
pub fn eventfd_signal(task: u64, fd: u64, value: u64) -> bool {
    let Some(target) = lookup(task, fd) else {
        return false;
    };
    match target {
        Target::EventFd { state, .. } => {
            let mut counter = state.lock();
            *counter = counter.saturating_add(value);
            true
        }
        _ => false,
    }
}

/// `procfs` fdinfo content for one descriptor: pos, octal flags and the
/// single mount id. `None` when the descriptor is not open (the caller
/// reports ENOENT like the real kernel does).
fn fdinfo_content(task: u64, fd: u64) -> Option<alloc::vec::Vec<u8>> {
    let flags = status_flags(task, fd)?;
    Some(
        alloc::format!(
            "pos:\t0\nflags:\t0{:o}\nmnt_id:\t1\n",
            flags
        )
        .into_bytes(),
    )
}

/// Whether the descriptor is an inotify instance.
pub fn is_inotify(task: u64, fd: u64) -> bool {
    matches!(lookup(task, fd), Some(Target::Inotify { .. }))
}

/// Whether the descriptor is the udmabuf controller.
pub fn is_udmabuf(task: u64, fd: u64) -> bool {
    matches!(lookup(task, fd), Some(Target::Udmabuf))
}

/// Whether the descriptor backs a dmabuf-compatible memory object.
pub fn is_memfd(task: u64, fd: u64) -> bool {
    matches!(lookup(task, fd), Some(Target::MemFd(_)))
}

/// `UDMABUF_CREATE`: wraps an existing memfd in a second descriptor that
/// acts as the dmabuf. The real kernel shares the pages; here both
/// descriptors reference the same `MemFdState`, so mmaps, reads and writes
/// stay in sync.
pub fn udmabuf_create(task: u64, memfd: u64, _offset: u64, size: u64) -> Result<u64, u64> {
    let rights = rights(task, memfd)?;
    require_right(task, memfd, FD_MMAP | FD_READ)?;
    let target = lookup(task, memfd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    if state.lock().len() < size as usize {
        return Err(EINVAL);
    }
    let fd = insert(task, Target::MemFd(state)).ok_or(EMFILE)?;
    with_table(task, |table| set_rights(table, fd, rights));
    Ok(fd)
}

/// Creates an anonymous in-memory file (`memfd_create`). File-backed `mmap`
/// is not implemented yet, so this is useful for read/write scratch storage.
pub fn memfd(task: u64) -> Result<u64, u64> {
    insert(
        task,
        Target::MemFd(Arc::new(Mutex::new(MemFdState {
            data: Vec::new(),
            len: 0,
            offset: 0,
            frames: None,
            maps: 0,
            seals: 0,
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
    require_right(task, epfd, FD_EVENT)?;
    require_right(task, fd, FD_POLL)?;
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
            epoll.entries.push(EpollEntry {
                fd,
                events,
                data,
                armed: true,
            });
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
            entry.armed = true;
            Ok(())
        }
        _ => Err(EINVAL),
    }
}

/// Level-triggered readiness translated from the poll bits.
const EPOLLONESHOT: u32 = 0x4000_0000;

fn epoll_ready_mask(task: u64, fd: u64, requested: u32) -> u32 {
    epoll_ready_mask_at(task, fd, requested, 0)
}

fn epoll_ready_mask_at(task: u64, fd: u64, requested: u32, depth: u8) -> u32 {
    if require_right(task, fd, FD_POLL).is_err() {
        return 0;
    }
    const EPOLLIN: u32 = 0x001;
    const EPOLLOUT: u32 = 0x004;
    const EPOLLERR: u32 = 0x008;
    const EPOLLHUP: u32 = 0x010;
    let Some(bits) = poll_ready_at(task, fd, depth) else {
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
    require_right(task, epfd, FD_EVENT)?;
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
            let mut epoll = state.lock();
            for entry in epoll.entries.iter_mut() {
                if ready.len() >= max_events {
                    break;
                }
                if !entry.armed {
                    continue;
                }
                let mask = epoll_ready_mask(task, entry.fd, entry.events);
                if mask != 0 {
                    ready.push((mask, entry.data));
                    if entry.events & EPOLLONESHOT != 0 {
                        entry.armed = false;
                    }
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
        let take = take.min(file.data.len().saturating_sub(file.offset));
        file.data[file.offset..file.offset + take].to_vec()
    };
    file.offset += take;
    drop(file);
    user_memory::copy_to_user(&chunk, buffer).map_err(|_| EFAULT)?;
    Ok(take as u64)
}

/// Copies a memfd's contents out for `execveat(AT_EMPTY_PATH)`. The caller
/// supplies the exec limit; anonymous files can otherwise be huge.
pub fn memfd_bytes(task: u64, fd: u64, limit: usize) -> Result<Vec<u8>, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_READ)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let file = state.lock();
    let length = file.len();
    if length > limit {
        return Err(EFBIG);
    }
    let mut bytes = alloc::vec![0_u8; length];
    if let Some(frames) = &file.frames {
        let source = crate::memory::phys_to_virt(frames.physical_start);
        unsafe {
            core::ptr::copy_nonoverlapping(source as *const u8, bytes.as_mut_ptr(), length);
        }
    } else {
        bytes.copy_from_slice(&file.data[..length]);
    }
    Ok(bytes)
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
    if file.frames.is_some() || end > MEMFD_SHADOW_LIMIT {
        // A live mapping fixes the run in place; growing it would move the
        // frames. Fail instead of silently writing into unmapped memory.
        if file.frames.is_none() {
            file.ensure_frames(end)?;
        }
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
    } else {
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
        file.data[start..end].copy_from_slice(&bytes);
    }
    if end > file.len {
        file.len = end;
    }
    file.offset = end;
    Ok(total as u64)
}

/// `ftruncate(2)` on a memfd or an ordinary writable file. Ordinary files
/// currently support only truncation to zero; other lengths fail explicitly.
pub fn truncate(task: u64, fd: u64, length: u64) -> Result<(), u64> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_WRITE | FD_TRUNCATE)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    if (length as i64) < 0 { return Err(EINVAL); }
    match target {
        Target::MemFd(_) => memfd_truncate(task, fd, length as usize),
        Target::Vfs(vfs_fd) => {
            if with_table(task, |table| access_at(table, fd)) == 0 { return Err(EINVAL); }
            if length != 0 { return Err(EINVAL); }
            vfs::truncate_zero(task, vfs_fd).map_err(|_| EINVAL)
        }
        _ => Err(EINVAL),
    }
}

/// `ftruncate(2)` / `fallocate(2)` on a memfd. The file is backed lazily; a
/// resize is refused while a shared mapping pins the current run.
pub fn memfd_truncate(task: u64, fd: u64, length: usize) -> Result<(), u64> {
    require_right(task, fd, FD_WRITE | FD_TRUNCATE)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let mut file = state.lock();
    if file.frames.is_some() || length > MEMFD_SHADOW_LIMIT {
        if file.frames.is_none() {
            file.ensure_frames(length)?;
        }
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
    } else {
        file.data.resize(length, 0);
    }
    file.len = length;
    Ok(())
}

/// `fcntl(F_ADD_SEALS)` on a memfd.
pub fn memfd_add_seals(task: u64, fd: u64, seals: u32) -> Result<(), u64> {
    require_right(task, fd, FD_WRITE)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let mut file = state.lock();
    // F_SEAL_SEAL is permanent once set.
    if file.seals & 0x1 != 0 {
        return Err(EPERM);
    }
    file.seals |= seals & 0x1f;
    Ok(())
}

/// `fcntl(F_GET_SEALS)` on a memfd.
pub fn memfd_get_seals(task: u64, fd: u64) -> Result<u32, u64> {
    require_right(task, fd, FD_FSTAT)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let seals = state.lock().seals;
    Ok(seals)
}

/// `fallocate(2)` with mode 0: grow the file if `offset + length` exceeds it.
pub fn memfd_fallocate(task: u64, fd: u64, offset: usize, length: usize) -> Result<(), u64> {
    require_right(task, fd, FD_WRITE)?;
    let needed = offset.checked_add(length).ok_or(EINVAL)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(EINVAL);
    };
    let current = state.lock().len();
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
    require_right(task, fd, FD_MMAP | FD_READ)?;
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
fn insert_transferred(
    task: u64,
    target: Target,
    rights: u32,
    ioctls: Option<Vec<u32>>,
) -> Option<u64> {
    match insert(task, target.clone()) {
        Some(fd) => {
            with_table(task, |table| {
                set_rights(table, fd, rights);
                set_ioctls(table, fd, ioctls);
            });
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
        Some(second_fd) => {
            Ok((first_fd, second_fd))
        }
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
/// Whether a bound AF_UNIX listener lives at `path`. The bind namespace is
/// kernel-side only (no VFS node is created), so the stat/chmod/unlink
/// syscalls that tooling runs against a socket path consult this registry.
pub fn listener_exists(path: &[u8]) -> bool {
    LISTENERS
        .lock()
        .iter()
        .any(|entry| entry.lock().path == path)
}

/// Removes a bound listener at `path` (unlink of a stale socket node).
pub fn listener_unlink(path: &[u8]) -> bool {
    let mut registry = LISTENERS.lock();
    let before = registry.len();
    registry.retain(|entry| entry.lock().path != path);
    registry.len() != before
}

pub fn bind(task: u64, fd: u64, path: Vec<u8>) -> Result<(), u64> {
    require_right(task, fd, FD_BIND)?;
    if path.is_empty() {
        return Err(EINVAL);
    }
    // A pathname bind publishes a name in the global namespace; abstract
    // names are the only ones that stay inside the already-held context.
    if path[0] != 0 && scheduler::current_task_capability_mode() {
        return Err(EPERM);
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
    require_right(task, fd, FD_LISTEN)?;
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
    require_right(task, fd, FD_CONNECT)?;
    if path.is_empty() {
        return Err(EINVAL);
    }
    if path[0] != 0 && scheduler::current_task_capability_mode() {
        return Err(EPERM);
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
    require_right(task, fd, FD_ACCEPT)?;
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
    fds: Vec<PassedFd>,
) -> Result<u64, u64> {
    let end = end as usize;
    let peer = 1 - end;
    let mut socket = state.lock();
    if socket.ends[end].write_closed || socket.ends[peer].read_closed {
        drop(socket);
        for (target, _, _) in &fds {
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
    require_right(task, fd, FD_SEND)?;
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::Socket { state, end, .. } = target else {
        return Err(ENOTSOCK);
    };
    let mut passed: Vec<PassedFd> = Vec::new();
    for &passed_fd in pass {
        let Some(passed_target) = lookup(task, passed_fd) else {
            for (target, _, _) in &passed {
                target.release_endpoint();
            }
            return Err(EBADF);
        };
        let Ok(passed_rights) = rights(task, passed_fd) else {
            for (target, _, _) in &passed {
                target.release_endpoint();
            }
            return Err(EBADF);
        };
        let passed_ioctls = with_table(task, |table| ioctls_at(table, passed_fd));
        passed_target.retain_endpoint();
        passed.push((passed_target, passed_rights, passed_ioctls));
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
) -> Result<(usize, Vec<PassedFd>), u64> {
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

/// Path of an open filesystem file, used to map it lazily. Synthetic
/// descriptors (memfd, pipes, devices) have no path and return `None`.
pub fn vfs_path(task: u64, fd: u64) -> Option<alloc::string::String> {
    let task = crate::scheduler::fd_table_owner(task);
    require_right(task, fd, FD_FSTAT).ok()?;
    match lookup(task, fd)? {
        Target::Vfs(vfs_fd) => crate::vfs::fd_path(task, vfs_fd),
        _ => None,
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
    require_right(task, fd, FD_RECV)?;
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
    for (target, rights, ioctls) in fds {
        if let Some(number) = insert_transferred(task, target, rights, ioctls) {
            numbers.push(number);
        }
    }
    Ok((read, numbers))
}

/// `shutdown(2)`. `how`: 0 read, 1 write, 2 both.
pub fn socket_shutdown(task: u64, fd: u64, how: u64) -> Result<(), u64> {
    require_right(task, fd, FD_SHUTDOWN)?;
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
        for (target, _, _) in message.fds {
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
    let file_access = with_table(task, |table| access_at(table, fd));
    let (access, nonblock) = match target {
        Target::Vfs(_) => (file_access, false),
        Target::Pipe { read, .. } => (if read { 0 } else { O_WRONLY }, false),
        Target::Socket { nonblock, .. }
        | Target::Unbound { nonblock }
        | Target::Listener { nonblock, .. }
        | Target::EventFd { nonblock, .. } => (O_RDWR, nonblock),
        Target::SignalFd(state) => (O_RDWR, state.lock().nonblock),
        Target::Netlink { nonblock } | Target::Inotify { nonblock } => (O_RDWR, nonblock),
        Target::TimerFd { nonblock, .. } => (O_RDWR, nonblock),
        Target::Console
        | Target::Null
        | Target::Zero
        | Target::Full
        | Target::Random
        | Target::Ivshmem
        | Target::GpuFb
        | Target::Drm { .. }
        | Target::Sysfs(_, _)
        | Target::Proc(_, _)
        | Target::DirList { .. }
        | Target::SysfsDir { .. }
        | Target::SysfsLink(_)
        | Target::Prime { .. }
        | Target::MemFd(_)
        | Target::Udmabuf
        | Target::Input(_)
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
            Some(Target::SignalFd(state)) => {
                state.lock().nonblock = nonblock;
                true
            }
            Some(Target::Netlink { nonblock: flag }) | Some(Target::Inotify { nonblock: flag }) => {
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
    require_right(task, fd, FD_POLL).ok()?;
    poll_ready_at(task, fd, 0)
}

fn poll_ready_at(task: u64, fd: u64, depth: u8) -> Option<i16> {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    let target = lookup(task, fd)?;
    Some(match target {
        Target::Console | Target::Null => POLLIN | POLLOUT,
        Target::Zero | Target::Random => POLLIN,
        Target::Input(index) => {
            crate::virtio_input::pump();
            if crate::virtio_input::available(index) > 0 {
                POLLIN
            } else {
                0
            }
        }
        Target::Ivshmem | Target::GpuFb => POLLIN | POLLOUT,
        Target::Sysfs(_, _)
        | Target::Proc(_, _)
        | Target::DirList { .. }
        | Target::SysfsDir { .. }
        | Target::SysfsLink(_) => POLLIN | POLLOUT,
        Target::Prime { .. } => POLLOUT,
        Target::Drm { .. } => {
            if crate::drm::event_pending() {
                POLLIN | POLLOUT
            } else {
                POLLOUT
            }
        }
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
        Target::SignalFd(state) => {
            if !state.lock().queued.is_empty() {
                POLLIN
            } else {
                0
            }
        }
        // No uevent publisher: the monitor never reports readable.
        Target::Netlink { .. } => 0,
        Target::Inotify { .. } => 0,
        Target::Udmabuf => POLLOUT,
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
            let pending = state.lock().pending.len();
            static LAST: core::sync::atomic::AtomicUsize =
                core::sync::atomic::AtomicUsize::new(0);
            let _previous = pending;
            if pending == 0 {
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
        // An epoll descriptor is readable only while one of its armed
        // entries is ready. Reporting it always ready makes a nested event
        // loop (calloop creates one) spin in `epoll_wait` and never service
        // the real descriptors. Copy the entries first so a malformed cycle
        // cannot deadlock on the same mutex; the depth cap bounds the walk.
        Target::Epoll(state) => {
            if depth >= 8 {
                0
            } else {
                let entries: Vec<(u64, u32, bool)> = state
                    .lock()
                    .entries
                    .iter()
                    .map(|entry| (entry.fd, entry.events, entry.armed))
                    .collect();
                let ready = entries.iter().any(|&(fd, events, armed)| {
                    armed && epoll_ready_mask_at(task, fd, events, depth + 1) != 0
                });
                if ready { POLLIN } else { 0 }
            }
        }
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
    let cloexec = source.cloexec.clone();
    let access = source.access.clone();
    let rights = source.rights.clone();
    let ioctls = source.ioctls.clone();
    tables.retain(|table| table.task != child);
    tables.push(Table {
        task: child,
        slots,
        access,
        rights,
        ioctls,
        cloexec,
    });
}

pub(crate) fn write_stat(buffer: u64, mode: u32, size: u64, ino: u64) -> Result<(), u64> {
    write_stat_rdev(buffer, mode, size, ino, 0)
}

/// Linux `makedev()` for the 64-bit dev_t encoding.
pub(crate) fn major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

pub(crate) fn minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

pub(crate) fn makedev(major: u32, minor: u32) -> u64 {
    ((major as u64 & 0xfff) << 8)
        | (minor as u64 & 0xff)
        | ((minor as u64 & !0xff) << 12)
        | ((major as u64 & !0xfff) << 32)
}

/// Creates an fd that refers to an exported BO range.
pub fn prime_fd(task: u64, pa: usize, size: usize) -> Result<u64, u64> {
    insert(task, Target::Prime { pa, size }).ok_or(EMFILE)
}

/// Physical range behind a PRIME fd, if `fd` is one.
pub fn prime_info(task: u64, fd: u64) -> Option<(usize, usize)> {
    let task = crate::scheduler::fd_table_owner(task);
    match lookup(task, fd) {
        Some(Target::Prime { pa, size }) => Some((pa, size)),
        _ => None,
    }
}

/// True when `fd` is a PRIME export.
/// Exports a KMS dumb buffer as a shared fd (a memfd backed by the same
/// contiguous frames, so the compositor's writes are visible through both).
/// This is the PRIME `HANDLE_TO_FD` path the GBM surface flow needs.
pub fn prime_export_dumb(task: u64, handle: u32) -> Result<u64, u64> {
    let (physical_start, byte_size) =
        crate::drm::dumb_buffer_info(handle).ok_or(ENOENT)?;
    let page_count = byte_size.div_ceil(4096);
    insert(
        task,
        Target::MemFd(Arc::new(Mutex::new(MemFdState {
            data: Vec::new(),
            len: page_count * 4096,
            offset: 0,
            frames: Some(MemFdFrames {
                physical_start,
                page_count,
                generation: 0,
            }),
            maps: 0,
            seals: 0,
        }))),
    )
    .ok_or(EMFILE)
}

/// The reverse direction: a previously exported buffer fd maps back to its
/// dumb GEM handle (`PRIME_FD_TO_HANDLE`).
pub fn prime_import_dumb(task: u64, fd: u64) -> Result<u32, u64> {
    let task = crate::scheduler::fd_table_owner(task);
    let target = lookup(task, fd).ok_or(EBADF)?;
    let Target::MemFd(state) = target else {
        return Err(ENOTSOCK);
    };
    let state = state.lock();
    let Some(frames) = &state.frames else {
        return Err(ENOENT);
    };
    crate::drm::dumb_handle_for_pa(frames.physical_start).ok_or(ENOENT)
}

pub fn is_prime(task: u64, fd: u64) -> bool {
    let task = crate::scheduler::fd_table_owner(task);
    matches!(lookup(task, fd), Some(Target::Prime { .. }))
}

/// True for the synthetic device nodes this kernel exposes under `/dev`.
pub fn device_exists(path: &str) -> bool {
    if let Some(name) = path.strip_prefix("/dev/dri/by-path/") {
        return crate::virtio_gpu::size().is_some()
            && (name.ends_with("-card") || name.ends_with("-render"));
    }
    if path
        .strip_prefix("/dev/tty")
        .map(|rest| rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
    {
        return true;
    }
    match path {
        "/dev/null" | "/dev/zero" | "/dev/full" | "/dev/urandom" | "/dev/random" => true,
        "/dev/console" | "/dev/tty" => true,
        "/dev/ivshmem" => true,
        "/dev/gpufb" => crate::virtio_gpu::size().is_some(),
        "/dev" => true,
        "/dev/input" => crate::virtio_input::present(),
        "/dev/input/event0" => crate::virtio_input::count() > 0,
        "/dev/input/event1" => crate::virtio_input::count() > 1,
        "/dev/dri" => crate::virtio_gpu::size().is_some(),
        "/dev/dri/card0" => crate::virtio_gpu::size().is_some(),
        "/dev/dri/renderD128" => crate::virtio_gpu::size().is_some(),
        _ => false,
    }
}

pub(crate) fn write_stat_rdev(
    buffer: u64,
    mode: u32,
    size: u64,
    ino: u64,
    rdev: u64,
) -> Result<(), u64> {
    let mut stat = [0_u8; 128];
    stat[8..16].copy_from_slice(&ino.to_le_bytes()); // st_ino
    stat[16..20].copy_from_slice(&mode.to_le_bytes()); // st_mode
    stat[20..24].copy_from_slice(&1_u32.to_le_bytes()); // st_nlink
    stat[32..40].copy_from_slice(&rdev.to_le_bytes()); // st_rdev
    stat[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    stat[56..60].copy_from_slice(&1024_u32.to_le_bytes()); // st_blksize
    stat[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // st_blocks
    user_memory::copy_to_user(&stat, buffer).map_err(|_| EFAULT)
}

const EPERM: u64 = 1;
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
const S_IFLNK: u32 = 0o120000;

/// Current task id helper used by `linux_abi`.
/// True when `fd` names the host-shared ivshmem window.
pub fn is_ivshmem(task: u64, fd: u64) -> bool {
    let task = crate::scheduler::fd_table_owner(task);
    matches!(lookup(task, fd), Some(Target::Ivshmem))
}

/// True when `fd` names the synthetic VT console (`/dev/tty0` included).
pub fn is_console(task: u64, fd: u64) -> bool {
    let task = crate::scheduler::fd_table_owner(task);
    matches!(lookup(task, fd), Some(Target::Console))
}

/// True when `fd` names the DRM/KMS card.
pub fn is_drm(task: u64, fd: u64) -> bool {
    let task = crate::scheduler::fd_table_owner(task);
    matches!(lookup(task, fd), Some(Target::Drm { .. }))
}

/// The evdev device index behind `fd`, if it is a `/dev/input/eventN` node.
pub fn input_device(task: u64, fd: u64) -> Option<usize> {
    let task = crate::scheduler::fd_table_owner(task);
    match lookup(task, fd) {
        Some(Target::Input(index)) => Some(index),
        _ => None,
    }
}

/// True when `fd` names the virtio-gpu scanout framebuffer.
pub fn is_gpufb(task: u64, fd: u64) -> bool {
    let task = crate::scheduler::fd_table_owner(task);
    matches!(lookup(task, fd), Some(Target::GpuFb))
}

pub fn current_task() -> u64 {
    scheduler::current_task_id()
}
