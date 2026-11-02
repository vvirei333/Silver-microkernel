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

use crate::{console, interrupts};

pub const MAX_NAME_LEN: usize = 32;
pub const MAX_PATH_LEN: usize = 191;
pub const OPEN_WRITE: u64 = 1;
pub const OPEN_APPEND: u64 = 2;

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
}

impl RamFs {
    /// Empty filesystem root, used for freshly mounted points.
    fn empty() -> Self {
        Self {
            root: Node::directory("/"),
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
        fs
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
                    data.clear();
                    Ok(())
                }
                NodeKind::Directory(_) => Err(()),
            };
        }

        children.push(Node::file(name));
        Ok(())
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        if children.iter().any(|node| node.name == name) {
            return Err(());
        }
        children.push(Node::directory(name));
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
        children.remove(index);
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
            if children.iter().any(|node| node.name == destination_name) {
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
        let remaining = data.len() - offset;
        if bytes.len() <= remaining {
            data[offset..offset + bytes.len()].copy_from_slice(bytes);
        } else {
            if remaining != 0 {
                data[offset..].copy_from_slice(&bytes[..remaining]);
            }
            data.extend_from_slice(&bytes[remaining..]);
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
        let components = rel_components(path)?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        if let Some(existing) = children.iter_mut().find(|node| node.name == name) {
            return match &mut existing.kind {
                NodeKind::File(data) => {
                    data.clear();
                    data.extend_from_slice(contents);
                    Ok(())
                }
                NodeKind::Directory(_) => Err(()),
            };
        }
        let mut node = Node::file(name);
        if let NodeKind::File(data) = &mut node.kind {
            data.extend_from_slice(contents);
        }
        children.push(node);
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
        // Never rename a mount point itself.
        if self
            .mounts
            .iter()
            .any(|mount| mount.point == source || mount.point == destination)
        {
            return Err(());
        }

        let is_directory = self.mounts[source_mount].fs.is_directory(&source_relative)?;
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

    fn read(&mut self, owner: u64, fd: u64, limit: usize) -> Result<Vec<u8>, ()> {
        let index = self
            .open_files
            .iter()
            .position(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        let path = self.open_files[index].path.clone();
        let offset = self.open_files[index].offset;
        let (mount, relative) = self.resolve(&path);
        let data = self.mounts[mount].fs.read_at(&relative, offset, limit)?;
        self.open_files[index].offset = offset + data.len();
        Ok(data)
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
    /// dynamic ELF loader, which needs the whole image in one buffer.
    fn read_file(&self, cwd: &str, path: &str) -> Result<Vec<u8>, ()> {
        let resolved = resolve_path(cwd, path)?;
        let (mount, relative) = self.resolve(&resolved);
        self.mounts[mount].fs.read_at(&relative, 0, usize::MAX)
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
        if flags != 0 || fstype != "ramfs" {
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
        self.mounts.push(Mount {
            point,
            fs: Box::new(RamFs::empty()),
        });
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

pub fn read(owner: u64, fd: u64, limit: usize) -> Result<Vec<u8>, ()> {
    with_vfs(|vfs| vfs.read(owner, fd, limit))
}

pub fn write(owner: u64, fd: u64, bytes: &[u8]) -> Result<usize, ()> {
    with_vfs(|vfs| vfs.write(owner, fd, bytes))
}

pub fn read_dir(cwd: &str, path: &str) -> Result<Vec<DirEntry>, ()> {
    with_vfs(|vfs| vfs.read_dir(cwd, path))
}

/// Reads a whole file by path (dynamic ELF loader).
pub fn read_file(cwd: &str, path: &str) -> Result<Vec<u8>, ()> {
    with_vfs(|vfs| vfs.read_file(cwd, path))
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

fn with_vfs<T>(call: impl FnOnce(&mut Vfs) -> T) -> T {
    let saved = interrupts::save_and_disable_irq();
    let result = call(vfs_slot().expect("VFS is not initialized"));
    interrupts::restore_irq(saved);
    result
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
