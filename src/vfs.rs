//! Mount-aware in-memory file tree and per-task file descriptors.
//!
//! The tree is a table of mounts. Index 0 is the root filesystem (`/`); every
//! other entry is a backend mounted at an absolute point. Path resolution
//! picks the **longest** matching mount point, so nested mounts shadow their
//! parent, and forwards the suffix to that backend. A backend only implements
//! node operations on paths relative to its own root — it never sees the
//! global mount table, which is what keeps new filesystems out of the VFS
//! dispatch code.
//!
//! Relative paths resolve from the calling task's cwd. Handles store absolute
//! normalized paths, so later chdir calls and directory Vec growth are safe.
//! Open handles and cwds pin a mount: `umount` refuses while either exists
//! under the point, so a backend is never dropped out from under a live path.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{console, interrupts, smp};

pub const MAX_NAME_LEN: usize = 32;
pub const MAX_PATH_LEN: usize = 191;
pub const OPEN_WRITE: u64 = 1;
pub const OPEN_APPEND: u64 = 2;

/// Hard cap for a single file in the in-memory VFS. The kernel heap is only
/// 2 MiB; a user-controlled file must never be able to consume an unbounded
/// share of it. 512 KiB leaves room for the read buffer, ELF image setup and
/// other kernel allocations.
pub const MAX_FILE_BYTES: usize = 512 * 1024;
/// System-wide budget for file payload **capacity**, summed over every RamFs
/// backend. Accounting the logical length was not enough: `Vec` growth keeps
/// spare capacity, so several backends could hold far more memory than
/// `MAX_VFS_BYTES` while all lengths summed below it.
pub const MAX_VFS_BYTES: usize = 1024 * 1024;
/// Payload capacity reserved by all live RamFs backends. Kept in one place so
/// a second mount cannot get a fresh per-backend allowance.
static VFS_PAYLOAD_BYTES: AtomicUsize = AtomicUsize::new(0);

fn payload_used() -> usize {
    VFS_PAYLOAD_BYTES.load(Ordering::Acquire)
}

/// Reserves `delta` more payload bytes if the system-wide budget allows it.
fn payload_try_grow(delta: usize) -> bool {
    if delta == 0 {
        return true;
    }
    let mut used = payload_used();
    loop {
        let Some(next) = used.checked_add(delta) else {
            return false;
        };
        if next > MAX_VFS_BYTES {
            return false;
        }
        match VFS_PAYLOAD_BYTES.compare_exchange_weak(
            used,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => used = actual,
        }
    }
}

/// Releases `delta` payload bytes after a file is truncated, removed, or its
/// backend is dropped.
fn payload_shrink(delta: usize) {
    if delta == 0 {
        return;
    }
    let mut used = payload_used();
    loop {
        let next = used.saturating_sub(delta);
        match VFS_PAYLOAD_BYTES.compare_exchange_weak(
            used,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(actual) => used = actual,
        }
    }
}

/// Applies a capacity change to the shared budget. Shrinking always succeeds;
/// growing fails before any caller state is modified when the budget is full.
fn payload_resize(old_capacity: usize, new_capacity: usize) -> bool {
    if new_capacity >= old_capacity {
        payload_try_grow(new_capacity - old_capacity)
    } else {
        payload_shrink(old_capacity - new_capacity);
        true
    }
}

/// Capacity to allocate when a file grows to `new_len`. A bounded
/// power-of-two target keeps reallocation logarithmic while still accounting
/// the real allocation rather than the logical length.
fn capacity_target(new_len: usize) -> Option<usize> {
    if new_len == 0 {
        return Some(0);
    }
    let target = new_len.next_power_of_two().min(MAX_FILE_BYTES);
    if target < new_len {
        None
    } else {
        Some(target)
    }
}
/// Upper bound on directory entries, so `read_dir` and directory growth stay
/// bounded regardless of how many files a task creates.
pub const MAX_DIR_ENTRIES: usize = 256;
/// Upper bound on nodes in one RamFs backend (files + directories).
pub const MAX_VFS_NODES: usize = 2048;
/// Per-task open-handle limit.
pub const MAX_OPEN_FILES_PER_TASK: usize = 32;
/// Global open-handle limit across all tasks; each handle also owns a path
/// String, so per-task limits alone are not enough to bound kernel memory.
pub const MAX_OPEN_FILES: usize = 128;
/// Upper bound on the global mount table (SILVERD is the only writer, but the
/// table itself is kernel memory and should still be finite).
pub const MAX_MOUNTS: usize = 16;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct DirEntry {
    pub name: [u8; MAX_NAME_LEN],
    pub name_len: u64,
    pub kind: u64,
}

/// A filesystem backend. Every path is normalized and **relative to this
/// backend's root** (`""` is the root itself); the VFS mount layer has already
/// stripped the mount-point prefix. Implementations never see the mount table.
pub trait FileSystem {
    fn create_file(&mut self, path: &str) -> Result<(), ()>;
    fn make_directory(&mut self, path: &str) -> Result<(), ()>;
    fn remove_file(&mut self, path: &str) -> Result<(), ()>;
    fn remove_directory(&mut self, path: &str) -> Result<(), ()>;
    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()>;
    fn is_directory(&self, path: &str) -> Result<bool, ()>;
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()>;
    /// Reads at most `limit` bytes starting at `offset` (clamped to EOF).
    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Result<Vec<u8>, ()>;
    /// Writes `bytes` at `offset` (or EOF when `append`). Returns
    /// `(written, new_offset)`.
    fn write_at(
        &mut self,
        path: &str,
        offset: usize,
        bytes: &[u8],
        append: bool,
    ) -> Result<(usize, usize), ()>;
    fn len(&self, path: &str) -> Result<usize, ()>;
    /// Bootstrap-only whole-file write (initramfs seeding).
    fn seed_file(&mut self, path: &str, contents: &[u8]) -> Result<(), ()>;
    /// Resources owned by `path` and everything below it, for quota checks.
    /// Backends that cannot account precisely return `None`; callers must
    /// treat that as "quota unknown" and fail closed where a quota applies.
    fn usage(&self, path: &str) -> Option<FsUsage> {
        let _ = path;
        None
    }
}

/// Backend-independent resource accounting for one managed subtree.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FsUsage {
    pub inodes: u32,
    pub data_blocks: u32,
    pub metadata_blocks: u32,
}

impl FsUsage {
    pub fn blocks(self) -> u32 {
        self.data_blocks.saturating_add(self.metadata_blocks)
    }
}

struct Node {
    name: String,
    kind: NodeKind,
}

enum NodeKind {
    File(Vec<u8>),
    Directory(Vec<Node>),
}

impl Node {
    fn file(name: &str) -> Self {
        Self {
            name: String::from(name),
            kind: NodeKind::File(Vec::new()),
        }
    }

    fn directory(name: &str) -> Self {
        Self {
            name: String::from(name),
            kind: NodeKind::Directory(Vec::new()),
        }
    }
}

/// The original in-memory tree, now one backend among potentially many.
pub struct RamFs {
    root: Node,
    /// Number of non-root nodes in this backend, used to cap total metadata.
    nodes: usize,
    /// Sum of `Vec` capacities of this backend's files (actual payload).
    bytes: usize,
}

impl RamFs {
    /// Empty filesystem root, used for freshly mounted points.
    fn empty() -> Self {
        Self {
            root: Node::directory("/"),
            nodes: 0,
            bytes: 0,
        }
    }

    /// Root filesystem with the historical `/bin`, `/etc`, `/tmp`.
    fn with_defaults() -> Self {
        let mut fs = Self::empty();
        if let NodeKind::Directory(children) = &mut fs.root.kind {
            children.push(Node::directory("bin"));
            children.push(Node::directory("etc"));
            children.push(Node::directory("tmp"));
        }
        fs.nodes = 3;
        fs
    }
}

impl Drop for RamFs {
    fn drop(&mut self) {
        // Unmounting drops the backend; its bytes must not stay reserved in
        // the system-wide budget. `self.bytes` is the sum of capacities that
        // were actually committed.
        payload_shrink(self.bytes);
    }
}

impl FileSystem for RamFs {
    fn create_file(&mut self, path: &str) -> Result<(), ()> {
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;

        if let Some(existing) = children.iter_mut().find(|node| node.name == name) {
            return match &mut existing.kind {
                NodeKind::File(data) => {
                    let old_capacity = data.capacity();
                    // Drop the allocation, not just the length: retained
                    // capacity is heap memory and is accounted as payload.
                    *data = Vec::new();
                    payload_shrink(old_capacity);
                    self.bytes = self.bytes.saturating_sub(old_capacity);
                    Ok(())
                }
                NodeKind::Directory(_) => Err(()),
            };
        }

        if self.nodes >= MAX_VFS_NODES || children.len() >= MAX_DIR_ENTRIES {
            return Err(());
        }
        if children.try_reserve(1).is_err() {
            return Err(());
        }
        children.push(Node::file(name));
        self.nodes += 1;
        Ok(())
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        if children.iter().any(|node| node.name == name) {
            return Err(());
        }
        if self.nodes >= MAX_VFS_NODES || children.len() >= MAX_DIR_ENTRIES {
            return Err(());
        }
        if children.try_reserve(1).is_err() {
            return Err(());
        }
        children.push(Node::directory(name));
        self.nodes += 1;
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), ()> {
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        let index = children
            .iter()
            .position(|node| node.name == name)
            .ok_or(())?;
        if !matches!(children[index].kind, NodeKind::File(_)) {
            return Err(());
        }
        let removed = children.remove(index);
        if let NodeKind::File(data) = removed.kind {
            let capacity = data.capacity();
            payload_shrink(capacity);
            self.bytes = self.bytes.saturating_sub(capacity);
        }
        self.nodes = self.nodes.saturating_sub(1);
        Ok(())
    }

    fn remove_directory(&mut self, path: &str) -> Result<(), ()> {
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        let index = children
            .iter()
            .position(|node| node.name == name)
            .ok_or(())?;
        match &children[index].kind {
            NodeKind::Directory(entries) if entries.is_empty() => {}
            _ => return Err(()),
        }
        children.remove(index);
        self.nodes = self.nodes.saturating_sub(1);
        Ok(())
    }

    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let source_components = rel_components(source)?;
        let (&source_name, source_parent) = source_components.split_last().ok_or(())?;
        let destination_components = rel_components(destination)?;
        let (&destination_name, destination_parent) =
            destination_components.split_last().ok_or(())?;

        let is_directory = match find_node(&self.root, &source_components).ok_or(())?.kind {
            NodeKind::File(_) => false,
            NodeKind::Directory(_) => true,
        };
        // A directory can never move into its own subtree.
        if is_directory && crate::vfs::path_is_under(destination, source) {
            return Err(());
        }

        // Validate the complete destination before detaching the source, so
        // every expected error leaves the tree unchanged.
        {
            let children = directory_mut(&mut self.root, destination_parent)?;
            if children.iter().any(|node| node.name == destination_name)
                || children.len() >= MAX_DIR_ENTRIES
            {
                return Err(());
            }
            // Reserve before detaching the source: a failed push after the
            // removal would lose the node.
            if children.try_reserve(1).is_err() {
                return Err(());
            }
        }
        let mut node = {
            let children = directory_mut(&mut self.root, source_parent)?;
            let index = children
                .iter()
                .position(|node| node.name == source_name)
                .ok_or(())?;
            children.remove(index)
        };
        node.name = String::from(destination_name);
        directory_mut(&mut self.root, destination_parent)
            .expect("validated rename destination disappeared")
            .push(node);
        Ok(())
    }

    fn is_directory(&self, path: &str) -> Result<bool, ()> {
        let components = rel_components(path)?;
        match &find_node(&self.root, &components).ok_or(())?.kind {
            NodeKind::Directory(_) => Ok(true),
            NodeKind::File(_) => Ok(false),
        }
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()> {
        let components = rel_components(path)?;
        let node = find_node(&self.root, &components).ok_or(())?;
        let children = match &node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return Err(()),
        };
        Ok(children
            .iter()
            .take(MAX_DIR_ENTRIES)
            .map(|node| {
                let mut entry = DirEntry {
                    name: [0; MAX_NAME_LEN],
                    name_len: node.name.len() as u64,
                    kind: match node.kind {
                        NodeKind::File(_) => 1,
                        NodeKind::Directory(_) => 2,
                    },
                };
                entry.name[..node.name.len()].copy_from_slice(node.name.as_bytes());
                entry
            })
            .collect())
    }

    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Result<Vec<u8>, ()> {
        let components = rel_components(path)?;
        let data = match &find_node(&self.root, &components).ok_or(())?.kind {
            NodeKind::File(data) => data,
            NodeKind::Directory(_) => return Err(()),
        };
        let start = offset.min(data.len());
        let count = limit.min(data.len() - start);
        Ok(data[start..start + count].to_vec())
    }

    fn write_at(
        &mut self,
        path: &str,
        offset: usize,
        bytes: &[u8],
        append: bool,
    ) -> Result<(usize, usize), ()> {
        let components = rel_components(path)?;
        let data = match &mut find_node_mut(&mut self.root, &components).ok_or(())?.kind {
            NodeKind::File(data) => data,
            NodeKind::Directory(_) => return Err(()),
        };
        // O_APPEND semantics: every write starts at the current EOF, even if
        // another handle extended the file after this one was opened.
        let offset = if append { data.len() } else { offset };
        if offset > data.len() {
            return Err(());
        }
        let Some(new_len) = offset.checked_add(bytes.len()) else {
            return Err(());
        };
        if new_len > MAX_FILE_BYTES {
            return Err(());
        }
        let old_len = data.len();
        let old_capacity = data.capacity();
        if new_len > old_capacity {
            // Growing: build the replacement allocation first, validate the
            // *actual* capacity against the shared budget, and only then swap.
            // A full budget leaves the file and both counters untouched.
            let Some(target) = capacity_target(new_len) else {
                return Err(());
            };
            let mut grown = Vec::new();
            if grown.try_reserve_exact(target).is_err() {
                return Err(());
            }
            let new_capacity = grown.capacity();
            if !payload_resize(old_capacity, new_capacity) {
                return Err(());
            }
            grown.extend_from_slice(&data[..old_len]);
            let remaining = old_len - offset;
            if bytes.len() <= remaining {
                grown[offset..offset + bytes.len()].copy_from_slice(bytes);
            } else {
                if remaining != 0 {
                    grown[offset..].copy_from_slice(&bytes[..remaining]);
                }
                grown.extend_from_slice(&bytes[remaining..]);
            }
            *data = grown;
            self.bytes = self
                .bytes
                .saturating_sub(old_capacity)
                .saturating_add(new_capacity);
        } else {
            // The write fits in already-accounted capacity: no allocation and
            // therefore no budget change.
            let remaining = old_len - offset;
            if bytes.len() <= remaining {
                data[offset..offset + bytes.len()].copy_from_slice(bytes);
            } else {
                if remaining != 0 {
                    data[offset..].copy_from_slice(&bytes[..remaining]);
                }
                data.extend_from_slice(&bytes[remaining..]);
            }
        }
        Ok((bytes.len(), offset + bytes.len()))
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let components = rel_components(path)?;
        match &find_node(&self.root, &components).ok_or(())?.kind {
            NodeKind::File(data) => Ok(data.len()),
            NodeKind::Directory(_) => Err(()),
        }
    }

    fn seed_file(&mut self, path: &str, contents: &[u8]) -> Result<(), ()> {
        if contents.len() > MAX_FILE_BYTES {
            return Err(());
        }
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        if let Some(existing) = children.iter_mut().find(|node| node.name == name) {
            return match &mut existing.kind {
                NodeKind::File(data) => {
                    let old_capacity = data.capacity();
                    let mut replacement = Vec::new();
                    if !contents.is_empty()
                        && replacement.try_reserve_exact(contents.len()).is_err()
                    {
                        return Err(());
                    }
                    let new_capacity = replacement.capacity();
                    if !payload_resize(old_capacity, new_capacity) {
                        return Err(());
                    }
                    replacement.extend_from_slice(contents);
                    *data = replacement;
                    self.bytes = self
                        .bytes
                        .saturating_sub(old_capacity)
                        .saturating_add(new_capacity);
                    Ok(())
                }
                NodeKind::Directory(_) => Err(()),
            };
        }
        if self.nodes >= MAX_VFS_NODES || children.len() >= MAX_DIR_ENTRIES {
            return Err(());
        }
        if children.try_reserve(1).is_err() {
            return Err(());
        }
        let mut node = Node::file(name);
        let mut data = Vec::new();
        if !contents.is_empty() && data.try_reserve_exact(contents.len()).is_err() {
            return Err(());
        }
        let capacity = data.capacity();
        if !payload_resize(0, capacity) {
            return Err(());
        }
        data.extend_from_slice(contents);
        if let NodeKind::File(slot) = &mut node.kind {
            *slot = data;
        }
        children.push(node);
        self.nodes += 1;
        self.bytes = self.bytes.saturating_add(capacity);
        Ok(())
    }
}

struct Mount {
    point: String,
    fs: Box<dyn FileSystem>,
}

struct OpenFile {
    owner: u64,
    fd: u64,
    path: String,
    offset: usize,
    writable: bool,
    append: bool,
}

struct Vfs {
    /// Index 0 is the root mount (`/`) and is never removed.
    mounts: Vec<Mount>,
    open_files: Vec<OpenFile>,
}

struct SharedVfs(UnsafeCell<Option<Vfs>>);

unsafe impl Sync for SharedVfs {}

static VFS: SharedVfs = SharedVfs(UnsafeCell::new(None));

impl Vfs {
    fn new() -> Self {
        let mut mounts = Vec::new();
        mounts.push(Mount {
            point: String::from("/"),
            fs: Box::new(RamFs::with_defaults()),
        });
        Self {
            mounts,
            open_files: Vec::new(),
        }
    }

    /// Resolves an absolute normalized path to `(mount index, suffix)`.
    /// The root mount always matches, so this cannot fail.
    fn resolve(&self, path: &str) -> (usize, String) {
        let points: Vec<&str> = self.mounts.iter().map(|mount| mount.point.as_str()).collect();
        match mount_match(&points, path) {
            Some((index, relative)) => (index, String::from(relative)),
            // Unreachable while the root mount exists; fail closed to the root.
            None => (0, String::from(path.trim_start_matches('/'))),
        }
    }

    fn create_file(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.create_file(&relative)
    }

    fn make_directory(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.make_directory(&relative)
    }

    fn remove_file(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        if self.open_files.iter().any(|file| file.path == resolved) {
            return Err(());
        }
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.remove_file(&relative)
    }

    fn remove_directory(
        &mut self,
        cwd: &str,
        path: &str,
        in_use: impl FnOnce(&str) -> bool,
    ) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        if self.mounts.iter().any(|mount| {
            mount.point != "/" && crate::vfs::path_is_under(&mount.point, &resolved)
        }) {
            return Err(());
        }
        if in_use(&resolved) {
            return Err(());
        }
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.remove_directory(&relative)
    }

    fn rename(
        &mut self,
        cwd: &str,
        source: &str,
        destination: &str,
        in_use: impl FnOnce(&str) -> bool,
    ) -> Result<(), ()> {
        let source = resolve_path(cwd, source)?;
        let destination = resolve_path(cwd, destination)?;
        if source == destination {
            return Err(());
        }
        let (source_mount, source_relative) = self.resolve(&source);
        let (destination_mount, destination_relative) = self.resolve(&destination);
        // Cross-device rename is not supported (EXDEV).
        if source_mount != destination_mount {
            return Err(());
        }
        let is_directory = self.mounts[source_mount].fs.is_directory(&source_relative)?;
        // Never rename a mount point itself, and never move a directory that
        // contains a mount point: otherwise the mount stays registered under
        // the old path and becomes unreachable (or aliases the new tree).
        if self.mounts.iter().any(|mount| {
            mount.point == source
                || mount.point == destination
                || (is_directory && crate::vfs::path_is_under(&mount.point, &source))
                || (is_directory && crate::vfs::path_is_under(&mount.point, &destination))
        }) {
            return Err(());
        }
        if is_directory && crate::vfs::path_is_under(&destination, &source) {
            return Err(());
        }
        // Handles contain absolute paths. Reject renaming an open file or any
        // directory containing one instead of leaving a stale handle behind.
        if self
            .open_files
            .iter()
            .any(|file| crate::vfs::path_is_under(&file.path, &source))
        {
            return Err(());
        }
        if is_directory && in_use(&source) {
            return Err(());
        }

        self.mounts[source_mount]
            .fs
            .rename(&source_relative, &destination_relative)
    }

    fn open(&mut self, owner: u64, cwd: &str, path: &str, flags: u64) -> Result<u64, ()> {
        if flags & !(OPEN_WRITE | OPEN_APPEND) != 0
            || (flags & OPEN_APPEND != 0 && flags & OPEN_WRITE == 0)
        {
            return Err(());
        }
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        if self.mounts[mount].fs.is_directory(&relative)? {
            return Err(());
        }
        let append = flags & OPEN_APPEND != 0;
        let offset = if append {
            self.mounts[mount].fs.len(&relative)?
        } else {
            0
        };

        if self
            .open_files
            .iter()
            .filter(|file| file.owner == owner)
            .count()
            >= MAX_OPEN_FILES_PER_TASK
            || self.open_files.len() >= MAX_OPEN_FILES
        {
            return Err(());
        }

        let mut fd = 3_u64;
        while self
            .open_files
            .iter()
            .any(|file| file.owner == owner && file.fd == fd)
        {
            fd = fd.checked_add(1).ok_or(())?;
        }
        self.open_files.push(OpenFile {
            owner,
            fd,
            path: resolved,
            offset,
            writable: flags & OPEN_WRITE != 0,
            append,
        });
        Ok(fd)
    }

    fn close(&mut self, owner: u64, fd: u64) -> Result<(), ()> {
        let index = self
            .open_files
            .iter()
            .position(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        self.open_files.remove(index);
        Ok(())
    }

    fn close_task(&mut self, owner: u64) {
        self.open_files.retain(|file| file.owner != owner);
    }

    fn open_count(&self, owner: u64) -> usize {
        self.open_files.iter().filter(|file| file.owner == owner).count()
    }

    /// Reads at the descriptor's current offset and commits the new offset
    /// only after `consume` succeeded. A copyout fault or a short/error path
    /// therefore leaves the descriptor unchanged, so the caller can retry the
    /// exact same bytes.
    fn read_with<F>(
        &mut self,
        owner: u64,
        fd: u64,
        limit: usize,
        consume: F,
    ) -> Result<usize, ()>
    where
        F: FnOnce(&[u8]) -> Result<(), ()>,
    {
        let index = self
            .open_files
            .iter()
            .position(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        let path = self.open_files[index].path.clone();
        let offset = self.open_files[index].offset;
        let (mount, relative) = self.resolve(&path);
        let data = self.mounts[mount].fs.read_at(&relative, offset, limit)?;
        // The visible side effect (copy_to_user) is part of the transaction:
        // any failure returns before the offset is touched.
        consume(&data)?;
        let new_offset = offset.checked_add(data.len()).ok_or(())?;
        self.open_files[index].offset = new_offset;
        Ok(data.len())
    }

    fn write(&mut self, owner: u64, fd: u64, bytes: &[u8]) -> Result<usize, ()> {
        let index = self
            .open_files
            .iter()
            .position(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        if !self.open_files[index].writable {
            return Err(());
        }
        let path = self.open_files[index].path.clone();
        let offset = self.open_files[index].offset;
        let append = self.open_files[index].append;
        let (mount, relative) = self.resolve(&path);
        let (written, new_offset) = self.mounts[mount]
            .fs
            .write_at(&relative, offset, bytes, append)?;
        self.open_files[index].offset = new_offset;
        Ok(written)
    }

    fn resolve_directory(&self, cwd: &str, path: &str) -> Result<String, ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        if self.mounts[mount].fs.is_directory(&relative)? {
            Ok(resolved)
        } else {
            Err(())
        }
    }

    fn read_dir(&self, cwd: &str, path: &str) -> Result<Vec<DirEntry>, ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.read_dir(&relative)
    }

    /// Returns the entire contents of a file, resolved by path. Used by the
    /// dynamic ELF loader, which needs the whole image in one buffer. The
    /// length is checked before the read so an oversized file cannot force an
    /// unbounded allocation.
    fn read_file(&self, cwd: &str, path: &str) -> Result<Vec<u8>, ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        if self.mounts[mount].fs.len(&relative)? > MAX_FILE_BYTES {
            return Err(());
        }
        self.mounts[mount]
            .fs
            .read_at(&relative, 0, MAX_FILE_BYTES)
    }

    /// Length lookup used by the syscall layer to reject an oversized `run`
    /// target before any file data is copied into kernel memory.
    fn file_len(&self, cwd: &str, path: &str) -> Result<usize, ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.len(&relative)
    }

    /// Resource accounting for the backend that owns `path`.
    fn usage(&self, cwd: &str, path: &str) -> Option<FsUsage> {
        let resolved = resolve_path(cwd, path).ok()?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.usage(&relative)
    }

    /// Creates (or overwrites) a file at `path` with `contents`. Bootstrap-only:
    /// the dynamic loader needs an executable in the tree before any program
    /// can write files into it.
    fn seed_file(&mut self, cwd: &str, path: &str, contents: &[u8]) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.seed_file(&relative, contents)
    }

    /// Mounts a new empty backend at `target`.
    ///
    /// `target` must already be a directory in the current tree, exactly one
    /// mount may occupy a point, and only the in-kernel `ramfs` backend is
    /// registered today. Unknown filesystem names fail closed (like ENODEV).
    fn mount(
        &mut self,
        cwd: &str,
        target: &str,
        fstype: &str,
        _source: &str,
        flags: u64,
        in_use: impl FnOnce(&str) -> bool,
    ) -> Result<(), ()> {
        if flags != 0 {
            return Err(());
        }
        if self.mounts.len() >= MAX_MOUNTS {
            return Err(());
        }
        let point = resolve_path(cwd, target)?;
        if point == "/" || self.mounts.iter().any(|mount| mount.point == point) {
            return Err(());
        }
        // The mount point must resolve to an existing directory in the tree
        // the mount is about to shadow.
        let (mount, relative) = self.resolve(&point);
        if !self.mounts[mount].fs.is_directory(&relative)? {
            return Err(());
        }
        // Handles and cwds store absolute paths: mounting over a live path
        // would silently retarget them into the new backend, so refuse.
        if self
            .open_files
            .iter()
            .any(|file| path_is_under(&file.path, &point))
            || in_use(&point)
        {
            return Err(());
        }
        let fs: Box<dyn FileSystem> = match fstype {
            "ramfs" => Box::new(RamFs::empty()),
            "sfs" => Box::new(crate::sfs::Sfs),
            _ => return Err(()),
        };
        self.mounts.push(Mount { point, fs });
        Ok(())
    }

    /// Unmounts the backend at `target`.
    ///
    /// Refuses while the point has child mounts, open files or a live cwd
    /// underneath it — the backend's memory must never outlive a path that
    /// still resolves into it.
    fn umount(
        &mut self,
        cwd: &str,
        target: &str,
        in_use: impl FnOnce(&str) -> bool,
    ) -> Result<(), ()> {
        let point = resolve_path(cwd, target)?;
        if point == "/" {
            return Err(());
        }
        let Some(index) = self.mounts.iter().position(|mount| mount.point == point) else {
            return Err(());
        };
        // Nested mounts keep the parent pinned (EBUSY).
        if self
            .mounts
            .iter()
            .any(|mount| mount.point != point && crate::vfs::path_is_under(&mount.point, &point))
        {
            return Err(());
        }
        if self
            .open_files
            .iter()
            .any(|file| crate::vfs::path_is_under(&file.path, &point))
        {
            return Err(());
        }
        if in_use(&point) {
            return Err(());
        }
        self.mounts.remove(index);
        Ok(())
    }
}

pub fn init() {
    unsafe { *VFS.0.get() = Some(Vfs::new()) };
}

pub fn create(cwd: &str, path: &str) -> Result<(), ()> {
    with_vfs(|vfs| vfs.create_file(cwd, path))
}

pub fn mkdir(cwd: &str, path: &str) -> Result<(), ()> {
    with_vfs(|vfs| vfs.make_directory(cwd, path))
}

/// Removes a closed file. Open handles keep path-based files alive until
/// closed; Unix-style unlink-while-open is intentionally not implemented.
pub fn unlink(cwd: &str, path: &str) -> Result<(), ()> {
    with_vfs(|vfs| vfs.remove_file(cwd, path))
}

/// Removes an empty directory only if no live task uses it as cwd or below it.
pub fn rmdir(cwd: &str, path: &str, in_use: impl FnOnce(&str) -> bool) -> Result<(), ()> {
    with_vfs(|vfs| vfs.remove_directory(cwd, path, in_use))
}

/// Renames without replacing an existing node. Open files and directories
/// used as a live task's cwd stay pinned while handles/cwds are path-based.
pub fn rename(
    cwd: &str,
    source: &str,
    destination: &str,
    in_use: impl FnOnce(&str) -> bool,
) -> Result<(), ()> {
    with_vfs(|vfs| vfs.rename(cwd, source, destination, in_use))
}

pub fn open(owner: u64, cwd: &str, path: &str, flags: u64) -> Result<u64, ()> {
    with_vfs(|vfs| vfs.open(owner, cwd, path, flags))
}

pub fn close(owner: u64, fd: u64) -> Result<(), ()> {
    with_vfs(|vfs| vfs.close(owner, fd))
}

pub fn close_task(owner: u64) {
    let saved = interrupts::save_and_disable_irq();
    if let Some(vfs) = vfs_slot() {
        vfs.close_task(owner);
    }
    interrupts::restore_irq(saved);
}

pub fn open_count(owner: u64) -> usize {
    with_vfs(|vfs| vfs.open_count(owner))
}

/// Transactional read: `consume` (the copyout) runs under the VFS lock and the
/// descriptor offset advances only when it returns `Ok`. Keeping the check,
/// copy and offset update in one critical section removes the read/advance
/// race as well as the EFAULT offset loss.
pub fn read_with<F>(owner: u64, fd: u64, limit: usize, consume: F) -> Result<usize, ()>
where
    F: FnOnce(&[u8]) -> Result<(), ()>,
{
    with_vfs(|vfs| vfs.read_with(owner, fd, limit, consume))
}

pub fn write(owner: u64, fd: u64, bytes: &[u8]) -> Result<usize, ()> {
    with_vfs(|vfs| vfs.write(owner, fd, bytes))
}

pub fn read_dir(cwd: &str, path: &str) -> Result<Vec<DirEntry>, ()> {
    with_vfs(|vfs| vfs.read_dir(cwd, path))
}

/// Reads a whole file by path (dynamic ELF loader). Rejects files larger than
/// [`MAX_FILE_BYTES`] before allocating the read buffer.
pub fn read_file(cwd: &str, path: &str) -> Result<Vec<u8>, ()> {
    with_vfs(|vfs| vfs.read_file(cwd, path))
}

/// Returns a file's length without reading it. Used to reject an oversized
/// `SYS_SPAWN_PATH` target before kernel memory is allocated.
pub fn file_len(cwd: &str, path: &str) -> Result<usize, ()> {
    with_vfs(|vfs| vfs.file_len(cwd, path))
}

/// Resource usage of `path` in whichever backend owns it. `None` means the
/// backend cannot account for that subtree.
pub fn usage(cwd: &str, path: &str) -> Option<FsUsage> {
    with_vfs(|vfs| vfs.usage(cwd, path))
}

/// Seeds a bootstrap file into the tree (initramfs-style, boot-time only).
pub fn seed_file(cwd: &str, path: &str, contents: &[u8]) -> Result<(), ()> {
    with_vfs(|vfs| vfs.seed_file(cwd, path, contents))
}

pub fn resolve_directory(cwd: &str, path: &str) -> Result<String, ()> {
    with_vfs(|vfs| vfs.resolve_directory(cwd, path))
}

/// Mounts a `ramfs` backend at `target`. `source` is accepted for ABI
/// forward-compatibility and ignored by the in-memory backend.
pub fn mount(
    cwd: &str,
    target: &str,
    fstype: &str,
    source: &str,
    flags: u64,
    in_use: impl FnOnce(&str) -> bool,
) -> Result<(), ()> {
    with_vfs(|vfs| vfs.mount(cwd, target, fstype, source, flags, in_use))
}

/// Unmounts the backend at `target`; refuses while it is still in use.
pub fn umount(cwd: &str, target: &str, in_use: impl FnOnce(&str) -> bool) -> Result<(), ()> {
    with_vfs(|vfs| vfs.umount(cwd, target, in_use))
}

/// Serializes every VFS entry point across CPUs.
///
/// Masking IRQs only protects the *local* PE: two CPUs issuing filesystem
/// syscalls would otherwise mutate the shared SFS image buffer and the single
/// virtio request slot concurrently. `SpinLock::lock` disables IRQs as well and
/// restores DAIF when the guard drops, so it replaces the manual mask.
///
/// Lock order: BKL -> `VFS_LOCK`. Nothing inside a VFS backend takes the BKL,
/// and no interrupt handler enters the VFS, so the order is acyclic.
static VFS_LOCK: smp::SpinLock<()> = smp::SpinLock::new(());

fn with_vfs<T>(call: impl FnOnce(&mut Vfs) -> T) -> T {
    let _guard = VFS_LOCK.lock();
    call(vfs_slot().expect("VFS is not initialized"))
}

fn vfs_slot() -> Option<&'static mut Vfs> {
    unsafe { &mut *VFS.0.get() }.as_mut()
}

fn resolve_path(cwd: &str, path: &str) -> Result<String, ()> {
    if path.is_empty() {
        return Err(());
    }
    let mut parts = if path.starts_with('/') {
        Vec::new()
    } else {
        components(cwd).ok_or(())?
    };
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name if name.len() <= MAX_NAME_LEN => parts.push(name),
            _ => return Err(()),
        }
    }

    let mut resolved = String::from("/");
    for (index, component) in parts.iter().enumerate() {
        if index != 0 {
            resolved.push('/');
        }
        resolved.push_str(component);
    }
    if resolved.len() > MAX_PATH_LEN {
        return Err(());
    }
    Ok(resolved)
}

fn components(path: &str) -> Option<Vec<&str>> {
    if path.is_empty() {
        return None;
    }
    let mut result = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                result.pop();
            }
            name if name.len() <= MAX_NAME_LEN => result.push(name),
            _ => return None,
        }
    }
    Some(result)
}

/// Components of a backend-relative path: no leading slash, no `..` (the VFS
/// resolver already normalized the absolute path). `""` is the backend root.
fn rel_components(path: &str) -> Result<Vec<&str>, ()> {
    let mut result = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(()),
            name if name.len() <= MAX_NAME_LEN => result.push(name),
            _ => return Err(()),
        }
    }
    if !result.is_empty() && result.iter().map(|name| name.len() + 1).sum::<usize>() > MAX_PATH_LEN {
        return Err(());
    }
    Ok(result)
}

fn directory_mut<'a>(mut node: &'a mut Node, path: &[&str]) -> Result<&'a mut Vec<Node>, ()> {
    for &component in path {
        let children = match &mut node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return Err(()),
        };
        node = children
            .iter_mut()
            .find(|child| child.name == component)
            .ok_or(())?;
    }
    match &mut node.kind {
        NodeKind::Directory(children) => Ok(children),
        NodeKind::File(_) => Err(()),
    }
}

fn find_node<'a>(root: &'a Node, path: &[&str]) -> Option<&'a Node> {
    let mut node = root;
    for &component in path {
        let children = match &node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return None,
        };
        node = children.iter().find(|child| child.name == component)?;
    }
    Some(node)
}

fn find_node_mut<'a>(root: &'a mut Node, path: &[&str]) -> Option<&'a mut Node> {
    let mut node = root;
    for &component in path {
        let children = match &mut node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return None,
        };
        node = children.iter_mut().find(|child| child.name == component)?;
    }
    Some(node)
}

// Re-export the pure helpers used above from the library crate so the bin and
// the host tests exercise the exact same matching logic.
pub use silver_microkernel::{mount_match, path_is_under};

/// Boot-time integration self-test for the mount layer. Exercises the real
/// global VFS (mount, nested mount, longest-prefix resolution, conflicts,
/// unknown filesystem, open-file pinning, unmount and compatibility of the
/// pre-existing operations). Best-effort cleanup runs even on failure.
#[cfg(not(feature = "tablet"))]
pub fn run_self_test() {
    let mut stage = 0_u32;
    let passed = self_test_inner(&mut stage);
    // Always return the tree to its pre-test shape.
    close_task(101);
    close_task(102);
    let _ = unlink("/", "/tmp/vfs_compat");
    let _ = umount("/", "/tmp/vfs_selftest/sub", |_| false);
    let _ = umount("/", "/tmp/vfs_selftest", |_| false);
    let _ = rmdir("/", "/tmp/vfs_selftest/sub", |_| false);
    let _ = rmdir("/", "/tmp/vfs_selftest", |_| false);
    if passed {
        console::write_str("VFS SELFTEST: PASS\r\n");
    } else {
        console::write_str("VFS SELFTEST: FAIL stage=");
        console::write_dec_u64(stage as u64);
        console::write_str("\r\n");
    }
}

#[cfg(not(feature = "tablet"))]
fn self_test_inner(stage: &mut u32) -> bool {
    *stage = 1;
    if mkdir("/", "/tmp/vfs_selftest").is_err() {
        return false;
    }
    *stage = 2;
    if mount("/", "/tmp/vfs_selftest", "ramfs", "selftest", 0, |_| false).is_err() {
        return false;
    }
    *stage = 3;
    // A second mount at the same point must be rejected.
    if mount("/", "/tmp/vfs_selftest", "ramfs", "selftest", 0, |_| false).is_ok() {
        return false;
    }
    *stage = 4;
    // Unknown filesystem must fail closed (ENODEV).
    if mount("/", "/tmp/vfs_selftest", "ext4", "selftest", 0, |_| false).is_ok() {
        return false;
    }
    *stage = 5;
    if create("/", "/tmp/vfs_selftest/file").is_err() {
        return false;
    }
    let fd = match open(0, "/", "/tmp/vfs_selftest/file", OPEN_WRITE) {
        Ok(fd) => fd,
        Err(_) => return false,
    };
    if write(0, fd, b"hello").is_err() || close(0, fd).is_err() {
        return false;
    }
    *stage = 6;
    match read_file("/", "/tmp/vfs_selftest/file") {
        Ok(data) if data == b"hello".to_vec() => {}
        _ => return false,
    }
    *stage = 7;
    // Per-task handle quota: one owner may hold at most 32 descriptors, but
    // another owner must still be able to open a descriptor. This also
    // exercises the owner check in `close_task` during cleanup above.
    let mut quota_fds = Vec::new();
    for _ in 0..MAX_OPEN_FILES_PER_TASK {
        match open(101, "/", "/tmp/vfs_selftest/file", 0) {
            Ok(fd) => quota_fds.push(fd),
            Err(_) => return false,
        }
    }
    if open(101, "/", "/tmp/vfs_selftest/file", 0).is_ok()
        || open(102, "/", "/tmp/vfs_selftest/file", 0).is_err()
    {
        return false;
    }
    for fd in quota_fds {
        if close(101, fd).is_err() {
            return false;
        }
    }
    close_task(102);
    *stage = 8;
    if mkdir("/", "/tmp/vfs_selftest/sub").is_err()
        || mount("/", "/tmp/vfs_selftest/sub", "ramfs", "selftest", 0, |_| false).is_err()
        || create("/", "/tmp/vfs_selftest/sub/nested").is_err()
    {
        return false;
    }
    *stage = 8;
    // Longest-prefix: the nested file lives in the child mount only, while
    // the parent file is still reachable through the parent mount.
    if read_file("/", "/tmp/vfs_selftest/sub/nested").is_err()
        || read_file("/", "/tmp/vfs_selftest/file").is_err()
    {
        return false;
    }
    *stage = 9;
    // A parent with a mounted child is busy.
    if umount("/", "/tmp/vfs_selftest", |_| false).is_ok() {
        return false;
    }
    *stage = 10;
    // An open handle pins its mount.
    let fd = match open(0, "/", "/tmp/vfs_selftest/sub/nested", OPEN_WRITE) {
        Ok(fd) => fd,
        Err(_) => return false,
    };
    if umount("/", "/tmp/vfs_selftest/sub", |_| false).is_ok() {
        return false;
    }
    if close(0, fd).is_err() {
        return false;
    }
    *stage = 11;
    if umount("/", "/tmp/vfs_selftest/sub", |_| false).is_err() {
        return false;
    }
    *stage = 12;
    // The shadowed directory created in the parent mount is visible again.
    if resolve_directory("/", "/tmp/vfs_selftest/sub").is_err() {
        return false;
    }
    if umount("/", "/tmp/vfs_selftest", |_| false).is_err() {
        return false;
    }
    *stage = 13;
    // The underlying root directory survives the mount, but the file that
    // lived in the now-dropped backend is gone.
    if resolve_directory("/", "/tmp/vfs_selftest").is_err()
        || read_file("/", "/tmp/vfs_selftest/file").is_ok()
    {
        return false;
    }
    // Unmounting a point with no mount must fail.
    if umount("/", "/tmp/vfs_selftest", |_| false).is_ok() {
        return false;
    }
    *stage = 14;
    // Existing operations on the root filesystem keep working.
    if create("/", "/tmp/vfs_compat").is_err()
        || read_file("/", "/tmp/vfs_compat").is_err()
        || unlink("/", "/tmp/vfs_compat").is_err()
    {
        return false;
    }
    true
}
