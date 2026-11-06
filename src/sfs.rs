//! Read-only SFS backend for the kernel VFS.
//!
//! The on-disk format is the one `format_sfs.py` creates and the EL0
//! `sfs_server` writes: 512-byte blocks, a superblock in block 0, a free-block
//! bitmap in block 1, a 128-entry inode table starting at block 2, and data
//! blocks from block 18.
//!
//! The kernel driver (`virtio_blk`) reads the whole 1 MiB image once, before
//! the EL0 block driver claims the device, and this backend serves reads from
//! that snapshot. Writes stay with the userspace server until the kernel owns
//! the write path; every mutating method fails closed here.

use alloc::vec::Vec;

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;
use crate::vfs::{DirEntry, FileSystem, MAX_NAME_LEN};
use crate::virtio_blk;

const MAGIC: u32 = 0x5346_5321;
const BLOCK_SIZE: usize = 512;
const INODE_SIZE: usize = 64;
const DIR_ENTRY_SIZE: usize = 32;
const DIR_ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / DIR_ENTRY_SIZE;
const NAME_MAX: usize = 27;
const DIRECT_BLOCKS: usize = 4;

const INODE_KIND_FILE: u32 = 1;
const INODE_KIND_DIR: u32 = 2;

/// Sector count of the on-disk image. The superblock is authoritative, but the
/// snapshot is taken before the superblock is parsed, so the format's fixed
/// geometry is used to size the read.
const IMAGE_SECTORS: usize = 2048;

static mut IMAGE: Option<(&'static [u8],)> = None;

/// Reads the whole SFS image into kernel memory. Must run before the EL0 block
/// driver initializes the device.
pub fn load() -> bool {
    if !virtio_blk::init() {
        console::write_str("KSFS: no block device\r\n");
        return false;
    }
    let bytes = IMAGE_SECTORS * BLOCK_SIZE;
    let Some((physical, _tag)) = frame_allocator::alloc_contiguous_pages_tagged(bytes / 4096) else {
        console::write_str("KSFS: image allocation failed\r\n");
        virtio_blk::reset();
        return false;
    };
    let base = phys_to_virt(physical);
    let mut sector = [0_u8; BLOCK_SIZE];
    for index in 0..IMAGE_SECTORS {
        if !virtio_blk::read_sector(index as u64, &mut sector) {
            console::write_str("KSFS: image read failed\r\n");
            virtio_blk::reset();
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                sector.as_ptr(),
                (base + index * BLOCK_SIZE) as *mut u8,
                BLOCK_SIZE,
            );
        }
    }
    virtio_blk::reset();
    let image: &'static [u8] = unsafe { core::slice::from_raw_parts(base as *const u8, bytes) };
    unsafe { IMAGE = Some((image,)) };
    true
}

fn image() -> Result<&'static [u8], ()> {
    unsafe {
        let slot = core::ptr::addr_of!(IMAGE) as *const Option<(&'static [u8],)>;
        match *slot {
            Some((bytes,)) => Ok(bytes),
            None => Err(()),
        }
    }
}

#[derive(Copy, Clone)]
struct Superblock {
    total_blocks: u32,
    root_inode: u32,
    inode_count: u32,
    inode_table_block: u32,
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn superblock(image: &[u8]) -> Result<Superblock, ()> {
    if image.len() < BLOCK_SIZE || u32_at(image, 0) != MAGIC {
        return Err(());
    }
    let block_size = u32_at(image, 8);
    let total_blocks = u32_at(image, 12);
    let root_inode = u32_at(image, 16);
    let inode_count = u32_at(image, 20);
    let inode_table_block = u32_at(image, 28);
    if block_size as usize != BLOCK_SIZE
        || total_blocks as usize * BLOCK_SIZE > image.len()
        || inode_count == 0
    {
        return Err(());
    }
    Ok(Superblock {
        total_blocks,
        root_inode,
        inode_count,
        inode_table_block,
    })
}

fn inode_offset(superblock: &Superblock, inode: u32) -> Option<usize> {
    if inode >= superblock.inode_count {
        return None;
    }
    Some(
        superblock.inode_table_block as usize * BLOCK_SIZE
            + inode as usize * INODE_SIZE,
    )
}

fn inode(image: &[u8], superblock: &Superblock, index: u32) -> Option<(u32, u32, [u32; 4])> {
    let offset = inode_offset(superblock, index)?;
    if offset + INODE_SIZE > image.len() {
        return None;
    }
    let kind = u32_at(image, offset);
    let size = u32_at(image, offset + 4);
    let mut blocks = [0_u32; DIRECT_BLOCKS];
    for (slot, block) in blocks.iter_mut().enumerate() {
        *block = u32_at(image, offset + 8 + slot * 4);
    }
    Some((kind, size, blocks))
}

/// Scans the root directory for `name`. Returns `(inode, kind, size)`.
fn lookup(image: &[u8], superblock: &Superblock, name: &str) -> Option<(u32, u32, u32)> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, blocks) = inode(image, superblock, superblock.root_inode)?;
    for block in blocks {
        if block == 0 {
            break;
        }
        let start = block as usize * BLOCK_SIZE;
        if start + BLOCK_SIZE > image.len() {
            return None;
        }
        for slot in 0..DIR_ENTRIES_PER_BLOCK {
            let entry = start + slot * DIR_ENTRY_SIZE;
            let index = u32_at(image, entry);
            if index == 0 {
                continue;
            }
            let length = image[entry + 4] as usize;
            if length != name.len() || length > NAME_MAX {
                continue;
            }
            if &image[entry + 5..entry + 5 + length] == name.as_bytes() {
                let (kind, size, _) = inode(image, superblock, index)?;
                return Some((index, kind, size));
            }
        }
    }
    None
}

/// Normalizes a backend-relative path: `""` and `/` are the root, everything
/// else must be a single component (nested directories come later).
fn component(path: &str) -> Result<&str, ()> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok("");
    }
    if trimmed.contains('/') {
        return Err(());
    }
    Ok(trimmed)
}

pub struct Sfs;

impl FileSystem for Sfs {
    fn create_file(&mut self, _path: &str) -> Result<(), ()> {
        Err(())
    }

    fn make_directory(&mut self, _path: &str) -> Result<(), ()> {
        Err(())
    }

    fn remove_file(&mut self, _path: &str) -> Result<(), ()> {
        Err(())
    }

    fn remove_directory(&mut self, _path: &str) -> Result<(), ()> {
        Err(())
    }

    fn rename(&mut self, _source: &str, _destination: &str) -> Result<(), ()> {
        Err(())
    }

    fn is_directory(&self, path: &str) -> Result<bool, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        let name = component(path)?;
        if name.is_empty() {
            return Ok(true);
        }
        let (_, kind, _) = lookup(image, &superblock, name).ok_or(())?;
        Ok(kind == INODE_KIND_DIR)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        if !component(path)?.is_empty() {
            return Err(());
        }
        let (_, _, blocks) = inode(image, &superblock, superblock.root_inode).ok_or(())?;
        let mut entries = Vec::new();
        for block in blocks {
            if block == 0 {
                break;
            }
            let start = block as usize * BLOCK_SIZE;
            if start + BLOCK_SIZE > image.len() {
                return Err(());
            }
            for slot in 0..DIR_ENTRIES_PER_BLOCK {
                let entry = start + slot * DIR_ENTRY_SIZE;
                let index = u32_at(image, entry);
                if index == 0 {
                    continue;
                }
                let length = image[entry + 4] as usize;
                if length == 0 || length > NAME_MAX {
                    continue;
                }
                let (kind, _, _) = inode(image, &superblock, index).ok_or(())?;
                let mut name = [0_u8; MAX_NAME_LEN];
                name[..length].copy_from_slice(&image[entry + 5..entry + 5 + length]);
                entries.push(DirEntry {
                    name,
                    name_len: length as u64,
                    kind: match kind {
                        INODE_KIND_DIR => 2,
                        _ => 1,
                    },
                });
            }
        }
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Result<Vec<u8>, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let (index, _, size) = lookup(image, &superblock, name).ok_or(())?;
        if offset as u32 >= size {
            return Ok(Vec::new());
        }
        let end = (offset.saturating_add(limit)).min(size as usize);
        let mut data = Vec::with_capacity(end - offset);
        let (_, _, blocks) = inode(image, &superblock, index).ok_or(())?;
        let mut position = offset;
        while position < end {
            let block_slot = position / BLOCK_SIZE;
            if block_slot >= DIRECT_BLOCKS {
                return Err(());
            }
            let block = blocks[block_slot];
            if block == 0 {
                return Err(());
            }
            let start = block as usize * BLOCK_SIZE;
            let within = position % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(end - position);
            if start + within + take > image.len() {
                return Err(());
            }
            data.extend_from_slice(&image[start + within..start + within + take]);
            position += take;
        }
        Ok(data)
    }

    fn write_at(
        &mut self,
        _path: &str,
        _offset: usize,
        _bytes: &[u8],
        _append: bool,
    ) -> Result<(usize, usize), ()> {
        Err(())
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        let name = component(path)?;
        if name.is_empty() {
            return Ok(0);
        }
        let (_, _, size) = lookup(image, &superblock, name).ok_or(())?;
        Ok(size as usize)
    }

    fn seed_file(&mut self, _path: &str, _contents: &[u8]) -> Result<(), ()> {
        Err(())
    }
}

/// True once `load` has captured an image.
pub fn loaded() -> bool {
    image().is_ok()
}

/// Boot probe for the snapshot: reports the superblock and the root entries it
/// can see. Purely diagnostic.
pub fn report() -> bool {
    let Ok(image) = image() else {
        return false;
    };
    let Ok(superblock) = superblock(image) else {
        console::write_str("KSFS: bad superblock\r\n");
        return false;
    };
    console::write_str("KSFS: mounted, blocks=");
    console::write_dec_u64(u64::from(superblock.total_blocks));
    console::write_str("\r\n");
    true
}
