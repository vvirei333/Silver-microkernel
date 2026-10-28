//! Single-CPU in-memory file tree and per-task file descriptors.
//!
//! Relative paths resolve from the calling task's cwd. Handles store absolute
//! normalized paths, so later chdir calls and directory Vec growth are safe.

use alloc::{string::String, vec::Vec};
use core::cell::UnsafeCell;

use crate::interrupts;

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

pub struct Node {
    name: String,
    kind: NodeKind,
}

enum NodeKind {
    File(Vec<u8>),
    Directory(Vec<Node>),
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
    root: Node,
    open_files: Vec<OpenFile>,
}

struct SharedVfs(UnsafeCell<Option<Vfs>>);

unsafe impl Sync for SharedVfs {}

static VFS: SharedVfs = SharedVfs(UnsafeCell::new(None));

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

impl Vfs {
    fn new() -> Self {
        let mut root = Node::directory("/");
        if let NodeKind::Directory(children) = &mut root.kind {
            children.push(Node::directory("bin"));
            children.push(Node::directory("etc"));
            children.push(Node::directory("tmp"));
        }
        Self {
            root,
            open_files: Vec::new(),
        }
    }

    fn create_file(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let components = components(&resolved).ok_or(())?;
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

    fn make_directory(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let components = components(&resolved).ok_or(())?;
        let (&name, parent) = components.split_last().ok_or(())?;
        let children = directory_mut(&mut self.root, parent)?;
        if children.iter().any(|node| node.name == name) {
            return Err(());
        }
        children.push(Node::directory(name));
        Ok(())
    }

    fn remove_file(&mut self, cwd: &str, path: &str) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        if self.open_files.iter().any(|file| file.path == resolved) {
            return Err(());
        }
        let components = components(&resolved).ok_or(())?;
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

    fn remove_directory(
        &mut self,
        cwd: &str,
        path: &str,
        in_use: impl FnOnce(&str) -> bool,
    ) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let components = components(&resolved).ok_or(())?;
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
        // Check every task while VFS IRQ masking also excludes chdir. This
        // prevents removing a directory out from under any task's cwd.
        if in_use(&resolved) {
            return Err(());
        }
        children.remove(index);
        Ok(())
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
        let source_components = components(&source).ok_or(())?;
        let (&source_name, source_parent) = source_components.split_last().ok_or(())?;
        let destination_components = components(&destination).ok_or(())?;
        let (&destination_name, destination_parent) =
            destination_components.split_last().ok_or(())?;

        let is_directory = match find_node(&self.root, &source).ok_or(())?.kind {
            NodeKind::File(_) => false,
            NodeKind::Directory(_) => true,
        };
        if is_directory
            && destination.starts_with(&source)
            && destination.as_bytes().get(source.len()) == Some(&b'/')
        {
            return Err(());
        }
        // Handles contain absolute paths. Reject renaming an open file or any
        // directory containing one instead of leaving a stale handle behind.
        if self.open_files.iter().any(|file| {
            file.path == source
                || (file.path.starts_with(&source)
                    && file.path.as_bytes().get(source.len()) == Some(&b'/'))
        }) {
            return Err(());
        }
        if is_directory && in_use(&source) {
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

    fn open(&mut self, owner: u64, cwd: &str, path: &str, flags: u64) -> Result<u64, ()> {
        if flags & !(OPEN_WRITE | OPEN_APPEND) != 0
            || (flags & OPEN_APPEND != 0 && flags & OPEN_WRITE == 0)
        {
            return Err(());
        }
        let resolved = resolve_path(cwd, path)?;
        let append = flags & OPEN_APPEND != 0;
        let offset = match &find_node(&self.root, &resolved).ok_or(())?.kind {
            NodeKind::File(data) => {
                if append {
                    data.len()
                } else {
                    0
                }
            }
            NodeKind::Directory(_) => return Err(()),
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
        let handle = self
            .open_files
            .iter_mut()
            .find(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        let node = find_node(&self.root, &handle.path).ok_or(())?;
        let data = match &node.kind {
            NodeKind::File(data) => data,
            NodeKind::Directory(_) => return Err(()),
        };
        let start = handle.offset.min(data.len());
        let count = limit.min(data.len() - start);
        let result = data[start..start + count].to_vec();
        handle.offset = start + count;
        Ok(result)
    }

    fn write(&mut self, owner: u64, fd: u64, bytes: &[u8]) -> Result<usize, ()> {
        let handle = self
            .open_files
            .iter_mut()
            .find(|file| file.owner == owner && file.fd == fd)
            .ok_or(())?;
        if !handle.writable {
            return Err(());
        }
        let node = find_node_mut(&mut self.root, &handle.path).ok_or(())?;
        let data = match &mut node.kind {
            NodeKind::File(data) => data,
            NodeKind::Directory(_) => return Err(()),
        };
        // O_APPEND semantics: every write starts at the current EOF, even if
        // another handle extended the file after this one was opened.
        if handle.append {
            handle.offset = data.len();
        }
        if handle.offset > data.len() {
            return Err(());
        }
        let remaining = data.len() - handle.offset;
        if bytes.len() <= remaining {
            data[handle.offset..handle.offset + bytes.len()].copy_from_slice(bytes);
        } else {
            if remaining != 0 {
                data[handle.offset..].copy_from_slice(&bytes[..remaining]);
            }
            data.extend_from_slice(&bytes[remaining..]);
        }
        handle.offset += bytes.len();
        Ok(bytes.len())
    }

    fn resolve_directory(&self, cwd: &str, path: &str) -> Result<String, ()> {
        let resolved = resolve_path(cwd, path)?;
        match find_node(&self.root, &resolved).ok_or(())?.kind {
            NodeKind::Directory(_) => Ok(resolved),
            NodeKind::File(_) => Err(()),
        }
    }

    fn read_dir(&self, cwd: &str, path: &str) -> Result<Vec<DirEntry>, ()> {
        let resolved = resolve_path(cwd, path)?;
        let node = find_node(&self.root, &resolved).ok_or(())?;
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

    /// Returns the entire contents of a file, resolved by path. Used by the
    /// dynamic ELF loader, which needs the whole image in one buffer.
    fn read_file(&self, cwd: &str, path: &str) -> Result<Vec<u8>, ()> {
        let resolved = resolve_path(cwd, path)?;
        match &find_node(&self.root, &resolved).ok_or(())?.kind {
            NodeKind::File(data) => Ok(data.clone()),
            NodeKind::Directory(_) => Err(()),
        }
    }

    /// Creates (or overwrites) a file at `path` with `contents`. Bootstrap-only:
    /// the dynamic loader needs an executable in the tree before any program
    /// can write files into it.
    fn seed_file(&mut self, cwd: &str, path: &str, contents: &[u8]) -> Result<(), ()> {
        let resolved = resolve_path(cwd, path)?;
        let components = components(&resolved).ok_or(())?;
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

fn find_node<'a>(root: &'a Node, path: &str) -> Option<&'a Node> {
    let components = components(path)?;
    let mut node = root;
    for component in components {
        let children = match &node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return None,
        };
        node = children.iter().find(|child| child.name == component)?;
    }
    Some(node)
}

fn find_node_mut<'a>(root: &'a mut Node, path: &str) -> Option<&'a mut Node> {
    let components = components(path)?;
    let mut node = root;
    for component in components {
        let children = match &mut node.kind {
            NodeKind::Directory(children) => children,
            NodeKind::File(_) => return None,
        };
        node = children.iter_mut().find(|child| child.name == component)?;
    }
    Some(node)
}
