//! Kernel SFS: the on-disk filesystem of the mobile OS.
//!
//! The on-disk format is the one `format_sfs.py` creates: 512-byte blocks, a
//! superblock in block 0, a free-block bitmap in block 1, a 128-entry inode
//! table starting at block 2, and data blocks from block 18.
//!
//! The kernel owns the disk (`virtio_blk`) and keeps the whole image in a
//! contiguous buffer. Reads are served from that buffer; every mutation is
//! written through to the device sector by sector, so the on-disk state stays
//! authoritative and survives a reboot.
//!
//! Scope of this step: the root directory, direct blocks (4 x 512 bytes per
//! file), create/truncate/append, mkdir, unlink and rename. Nested directories
//! and indirect blocks are the next milestone.

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

const INODE_KIND_FREE: u32 = 0;
const INODE_KIND_FILE: u32 = 1;
const INODE_KIND_DIR: u32 = 2;

/// The format's fixed geometry; used to size the boot read before the
/// superblock is parsed.
const IMAGE_SECTORS: usize = 2048;

static mut IMAGE_PTR: *mut u8 = core::ptr::null_mut();
static mut IMAGE_LEN: usize = 0;

#[derive(Copy, Clone)]
struct Superblock {
    total_blocks: u32,
    root_inode: u32,
    inode_count: u32,
    inode_table_block: u32,
    bitmap_block: u32,
    data_block_start: u32,
}

fn image() -> Result<&'static [u8], ()> {
    unsafe {
        if IMAGE_PTR.is_null() {
            return Err(());
        }
        Ok(core::slice::from_raw_parts(IMAGE_PTR, IMAGE_LEN))
    }
}

fn image_mut() -> Result<&'static mut [u8], ()> {
    unsafe {
        if IMAGE_PTR.is_null() {
            return Err(());
        }
        Ok(core::slice::from_raw_parts_mut(IMAGE_PTR, IMAGE_LEN))
    }
}

pub fn loaded() -> bool {
    unsafe { !IMAGE_PTR.is_null() }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn superblock(image: &[u8]) -> Result<Superblock, ()> {
    if image.len() < BLOCK_SIZE || u32_at(image, 0) != MAGIC {
        return Err(());
    }
    let block_size = u32_at(image, 8);
    let total_blocks = u32_at(image, 12);
    let root_inode = u32_at(image, 16);
    let inode_count = u32_at(image, 20);
    let bitmap_block = u32_at(image, 24);
    let inode_table_block = u32_at(image, 28);
    let data_block_start = u32_at(image, 36);
    // Every metadata bound is checked here, before any of these fields can
    // become an image offset. A corrupt or hostile superblock must fail the
    // mount instead of panicking the kernel on the first filesystem call.
    let bitmap_end = (bitmap_block as usize)
        .saturating_mul(BLOCK_SIZE)
        .saturating_add((total_blocks as usize).div_ceil(8));
    let inode_end = (inode_table_block as usize)
        .saturating_mul(BLOCK_SIZE)
        .saturating_add((inode_count as usize).saturating_mul(INODE_SIZE));
    if block_size as usize != BLOCK_SIZE
        || total_blocks as usize * BLOCK_SIZE > image.len()
        || inode_count == 0
        || data_block_start >= total_blocks
        || bitmap_block >= total_blocks
        || inode_table_block >= total_blocks
        || bitmap_end > image.len()
        || inode_end > image.len()
        || root_inode >= inode_count
    {
        return Err(());
    }
    Ok(Superblock {
        total_blocks,
        root_inode,
        inode_count,
        inode_table_block,
        bitmap_block,
        data_block_start,
    })
}

fn inode_offset(superblock: &Superblock, inode: u32) -> Option<usize> {
    if inode >= superblock.inode_count {
        return None;
    }
    Some(superblock.inode_table_block as usize * BLOCK_SIZE + inode as usize * INODE_SIZE)
}

/// `inode_offset` plus the image-bounds check, for callers that read the
/// inode through [`u32_at`] instead of [`inode_get`].
fn inode_offset_checked(
    image: &[u8],
    superblock: &Superblock,
    inode: u32,
) -> Option<usize> {
    let offset = inode_offset(superblock, inode)?;
    if offset + INODE_SIZE > image.len() {
        return None;
    }
    Some(offset)
}

/// Validates a data-block number read from an inode or a directory entry
/// before it is used as an image offset. Metadata blocks, out-of-range
/// numbers and blocks that fall outside the loaded image are rejected, so a
/// corrupt image yields `None` instead of a slice panic.
fn block_start(image: &[u8], superblock: &Superblock, block: u32) -> Option<usize> {
    if block < superblock.data_block_start || block >= superblock.total_blocks {
        return None;
    }
    let start = (block as usize).checked_mul(BLOCK_SIZE)?;
    if start.checked_add(BLOCK_SIZE)? > image.len() {
        return None;
    }
    Some(start)
}

fn inode_get(
    image: &[u8],
    superblock: &Superblock,
    index: u32,
) -> Option<(u32, u32, [u32; DIRECT_BLOCKS])> {
    let offset = inode_offset_checked(image, superblock, index)?;
    let kind = u32_at(image, offset);
    let size = u32_at(image, offset + 4);
    let mut blocks = [0_u32; DIRECT_BLOCKS];
    for (slot, block) in blocks.iter_mut().enumerate() {
        *block = u32_at(image, offset + 8 + slot * 4);
    }
    Some((kind, size, blocks))
}

fn inode_put(
    image: &mut [u8],
    superblock: &Superblock,
    index: u32,
    kind: u32,
    size: u32,
    blocks: &[u32; DIRECT_BLOCKS],
) -> bool {
    let Some(offset) = inode_offset(superblock, index) else {
        return false;
    };
    if offset + INODE_SIZE > image.len() {
        return false;
    }
    put_u32(image, offset, kind);
    put_u32(image, offset + 4, size);
    for (slot, &block) in blocks.iter().enumerate() {
        put_u32(image, offset + 8 + slot * 4, block);
    }
    flush_block(image, superblock, offset / BLOCK_SIZE)
}

fn bitmap_used(image: &[u8], superblock: &Superblock, block: u32) -> bool {
    let base = superblock.bitmap_block as usize * BLOCK_SIZE;
    let byte = base + (block as usize) / 8;
    if byte >= image.len() {
        return true;
    }
    image[byte] & (1 << (block % 8)) != 0
}

fn bitmap_set(image: &mut [u8], superblock: &Superblock, block: u32, used: bool) {
    let base = superblock.bitmap_block as usize * BLOCK_SIZE;
    let byte = base + (block as usize) / 8;
    if byte >= image.len() {
        return;
    }
    if used {
        image[byte] |= 1 << (block % 8);
    } else {
        image[byte] &= !(1 << (block % 8));
    }
}

fn flush_block(image: &[u8], superblock: &Superblock, block: usize) -> bool {
    let start = block * BLOCK_SIZE;
    if start + BLOCK_SIZE > image.len() {
        return false;
    }
    let mut sector = [0_u8; BLOCK_SIZE];
    sector.copy_from_slice(&image[start..start + BLOCK_SIZE]);
    let _ = superblock;
    virtio_blk::write_sector(block as u64, &sector)
}

fn flush_bitmap(image: &[u8], superblock: &Superblock) -> bool {
    flush_block(image, superblock, superblock.bitmap_block as usize)
}

fn alloc_block(image: &mut [u8], superblock: &Superblock) -> Option<u32> {
    for block in superblock.data_block_start..superblock.total_blocks {
        if !bitmap_used(image, superblock, block) {
            bitmap_set(image, superblock, block, true);
            return Some(block);
        }
    }
    None
}

fn free_blocks(image: &mut [u8], superblock: &Superblock, blocks: &[u32; DIRECT_BLOCKS]) {
    for &block in blocks {
        // Never let a corrupt inode release metadata into the free pool.
        if block >= superblock.data_block_start && block < superblock.total_blocks {
            bitmap_set(image, superblock, block, false);
        }
    }
}

/// Finds a root-directory entry by name: `(inode, kind, size)`.
fn lookup(image: &[u8], superblock: &Superblock, name: &str) -> Option<(u32, u32, u32)> {
    let (block, slot) = find_entry(image, superblock, name)?;
    let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
    let (kind, size, _) = inode_get(image, superblock, index)?;
    Some((index, kind, size))
}

/// Locates the 32-byte directory slot holding `name`: `(block, slot)`.
fn find_entry(image: &[u8], superblock: &Superblock, name: &str) -> Option<(usize, usize)> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, blocks) = inode_get(image, superblock, superblock.root_inode)?;
    for block in blocks {
        if block == 0 {
            break;
        }
        let start = block_start(image, superblock, block)?;
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
                return Some((block as usize, slot));
            }
        }
    }
    None
}

fn find_free_inode(image: &[u8], superblock: &Superblock) -> Option<u32> {
    for index in 1..superblock.inode_count {
        let offset = inode_offset_checked(image, superblock, index)?;
        if u32_at(image, offset) == INODE_KIND_FREE {
            return Some(index);
        }
    }
    None
}

/// Adds `name -> child` to the root directory, allocating a directory block
/// when every existing slot is taken.
fn add_entry(
    image: &mut [u8],
    superblock: &Superblock,
    name: &str,
    child: u32,
) -> Option<()> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let root = superblock.root_inode;
    let (_, _, mut blocks) = inode_get(image, superblock, root)?;

    for &block in blocks.iter() {
        if block == 0 {
            break;
        }
        let start = block_start(image, superblock, block)?;
        for slot in 0..DIR_ENTRIES_PER_BLOCK {
            let entry = start + slot * DIR_ENTRY_SIZE;
            if u32_at(image, entry) == 0 {
                put_u32(image, entry, child);
                image[entry + 4] = name.len() as u8;
                image[entry + 5..entry + 5 + name.len()].copy_from_slice(name.as_bytes());
                for byte in &mut image[entry + 5 + name.len()..entry + DIR_ENTRY_SIZE] {
                    *byte = 0;
                }
                if !flush_block(image, superblock, block as usize) {
                    return None;
                }
                return Some(());
            }
        }
    }

    let block = alloc_block(image, superblock)?;
    let Some(start) = block_start(image, superblock, block) else {
        bitmap_set(image, superblock, block, false);
        return None;
    };
    for byte in &mut image[start..start + BLOCK_SIZE] {
        *byte = 0;
    }
    put_u32(image, start, child);
    image[start + 4] = name.len() as u8;
    image[start + 5..start + 5 + name.len()].copy_from_slice(name.as_bytes());

    let mut placed = false;
    for slot in blocks.iter_mut() {
        if *slot == 0 {
            *slot = block;
            placed = true;
            break;
        }
    }
    if !placed {
        bitmap_set(image, superblock, block, false);
        return None;
    }
    if !flush_block(image, superblock, block as usize) {
        return None;
    }
    if !inode_put(image, superblock, root, INODE_KIND_DIR, 0, &blocks) {
        return None;
    }
    flush_bitmap(image, superblock);
    Some(())
}

fn create_inode(
    image: &mut [u8],
    superblock: &Superblock,
    name: &str,
    kind: u32,
) -> Option<u32> {
    let index = find_free_inode(image, superblock)?;
    let empty = [0_u32; DIRECT_BLOCKS];
    if !inode_put(image, superblock, index, kind, 0, &empty) {
        return None;
    }
    if add_entry(image, superblock, name, index).is_none() {
        let _ = inode_put(image, superblock, index, INODE_KIND_FREE, 0, &empty);
        return None;
    }
    flush_bitmap(image, superblock);
    Some(index)
}

fn truncate(image: &mut [u8], superblock: &Superblock, index: u32) -> bool {
    let Some((kind, _, blocks)) = inode_get(image, superblock, index) else {
        return false;
    };
    free_blocks(image, superblock, &blocks);
    let empty = [0_u32; DIRECT_BLOCKS];
    let ok = inode_put(image, superblock, index, kind, 0, &empty);
    flush_bitmap(image, superblock);
    ok
}

fn store(
    image: &mut [u8],
    superblock: &Superblock,
    index: u32,
    bytes: &[u8],
    replacing: &[u32; DIRECT_BLOCKS],
) -> bool {
    let blocks_needed = bytes.len().div_ceil(BLOCK_SIZE);
    if blocks_needed > DIRECT_BLOCKS {
        return false;
    }
    let mut blocks = [0_u32; DIRECT_BLOCKS];
    for slot in blocks.iter_mut().take(blocks_needed) {
        let Some(block) = alloc_block(image, superblock) else {
            free_blocks(image, superblock, &blocks);
            return false;
        };
        *slot = block;
    }
    // Persist the allocation *before* the inode can point at these blocks.
    // A crash after this point leaks blocks (recoverable); the reverse order
    // would let the allocator hand an inode-owned block to another file and
    // cross-link their data.
    if !flush_bitmap(image, superblock) {
        return false;
    }
    for (offset, &block) in blocks.iter().enumerate().take(blocks_needed) {
        let start = offset * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(bytes.len());
        let mut sector = [0_u8; BLOCK_SIZE];
        sector[..end - start].copy_from_slice(&bytes[start..end]);
        let Some(image_start) = block_start(image, superblock, block) else {
            return false;
        };
        image[image_start..image_start + BLOCK_SIZE].copy_from_slice(&sector);
        if !virtio_blk::write_sector(block as u64, &sector) {
            return false;
        }
    }
    if !inode_put(image, superblock, index, INODE_KIND_FILE, bytes.len() as u32, &blocks) {
        return false;
    }
    // The inode no longer references the old blocks, so they can be released.
    // Skip anything that ended up in the new set (only possible if the bitmap
    // was already inconsistent before this call).
    for &block in replacing {
        if block != 0 && !blocks.contains(&block) {
            bitmap_set(image, superblock, block, false);
        }
    }
    flush_bitmap(image, superblock);
    true
}

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
    fn create_file(&mut self, path: &str) -> Result<(), ()> {
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        match lookup(image, &superblock, name) {
            Some((index, INODE_KIND_FILE, _)) => {
                if truncate(image, &superblock, index) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            Some(_) => Err(()),
            None => create_inode(image, &superblock, name, INODE_KIND_FILE)
                .map(|_| ())
                .ok_or(()),
        }
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        if lookup(image, &superblock, name).is_some() {
            return Err(());
        }
        create_inode(image, &superblock, name, INODE_KIND_DIR)
            .map(|_| ())
            .ok_or(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), ()> {
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let (index, kind, _) = lookup(image, &superblock, name).ok_or(())?;
        if kind != INODE_KIND_FILE {
            return Err(());
        }
        let Some((block, slot)) = find_entry(image, &superblock, name) else {
            return Err(());
        };
        let (_, _, blocks) = inode_get(image, &superblock, index).ok_or(())?;
        free_blocks(image, &superblock, &blocks);
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        if !flush_block(image, &superblock, block) {
            return Err(());
        }
        let empty = [0_u32; DIRECT_BLOCKS];
        inode_put(image, &superblock, index, INODE_KIND_FREE, 0, &empty);
        flush_bitmap(image, &superblock);
        Ok(())
    }

    fn remove_directory(&mut self, path: &str) -> Result<(), ()> {
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let (index, kind, _) = lookup(image, &superblock, name).ok_or(())?;
        if kind != INODE_KIND_DIR {
            return Err(());
        }
        let (_, _, blocks) = inode_get(image, &superblock, index).ok_or(())?;
        if blocks.iter().any(|&block| block != 0) {
            return Err(());
        }
        let Some((block, slot)) = find_entry(image, &superblock, name) else {
            return Err(());
        };
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        if !flush_block(image, &superblock, block) {
            return Err(());
        }
        let empty = [0_u32; DIRECT_BLOCKS];
        inode_put(image, &superblock, index, INODE_KIND_FREE, 0, &empty);
        flush_bitmap(image, &superblock);
        Ok(())
    }

    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let old = component(source)?;
        let new = component(destination)?;
        if old.is_empty() || new.is_empty() || new.len() > NAME_MAX {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        if lookup(image, &superblock, new).is_some() {
            return Err(());
        }
        let Some((block, slot)) = find_entry(image, &superblock, old) else {
            return Err(());
        };
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        image[entry + 4] = new.len() as u8;
        image[entry + 5..entry + 5 + new.len()].copy_from_slice(new.as_bytes());
        for byte in &mut image[entry + 5 + new.len()..entry + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        if !flush_block(image, &superblock, block) {
            return Err(());
        }
        Ok(())
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
        let (_, _, blocks) = inode_get(image, &superblock, superblock.root_inode).ok_or(())?;
        let mut entries = Vec::new();
        for block in blocks {
            if block == 0 {
                break;
            }
            let start = block_start(image, &superblock, block).ok_or(())?;
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
                let (kind, _, _) = inode_get(image, &superblock, index).ok_or(())?;
                let mut name = [0_u8; MAX_NAME_LEN];
                name[..length].copy_from_slice(&image[entry + 5..entry + 5 + length]);
                entries.push(DirEntry {
                    name,
                    name_len: length as u64,
                    kind: if kind == INODE_KIND_DIR { 2 } else { 1 },
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
        if offset >= size as usize {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(limit).min(size as usize);
        let (_, _, blocks) = inode_get(image, &superblock, index).ok_or(())?;
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let slot = position / BLOCK_SIZE;
            if slot >= DIRECT_BLOCKS || blocks[slot] == 0 {
                return Err(());
            }
            let start = block_start(image, &superblock, blocks[slot]).ok_or(())?;
            let within = position % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(end - position);
            data.extend_from_slice(&image[start + within..start + within + take]);
            position += take;
        }
        Ok(data)
    }

    fn write_at(
        &mut self,
        path: &str,
        offset: usize,
        bytes: &[u8],
        append: bool,
    ) -> Result<(usize, usize), ()> {
        let name = component(path)?;
        if name.is_empty() {
            return Err(());
        }
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let (index, _, size) = match lookup(image, &superblock, name) {
            Some((index, INODE_KIND_FILE, size)) => (index, INODE_KIND_FILE, size),
            Some(_) => return Err(()),
            None => (
                create_inode(image, &superblock, name, INODE_KIND_FILE).ok_or(())?,
                INODE_KIND_FILE,
                u32::from(0_u8),
            ),
        };
        let _ = size;

        let base = if append {
            lookup(image, &superblock, name).map(|(_, _, size)| size as usize).unwrap_or(0)
        } else {
            if offset != 0 {
                return Err(());
            }
            if !truncate(image, &superblock, index) {
                return Err(());
            }
            0
        };

        if append {
            let mut existing = Vec::new();
            if base > 0 {
                let (_, _, blocks) = inode_get(image, &superblock, index).ok_or(())?;
                let size = base;
                let mut position = 0;
                while position < size {
                    let slot = position / BLOCK_SIZE;
                    if slot >= DIRECT_BLOCKS || blocks[slot] == 0 {
                        return Err(());
                    }
                    let start = block_start(image, &superblock, blocks[slot]).ok_or(())?;
                    let within = position % BLOCK_SIZE;
                    let take = (BLOCK_SIZE - within).min(size - position);
                    existing.extend_from_slice(&image[start + within..start + within + take]);
                    position += take;
                }
            }
            existing.extend_from_slice(bytes);
            let (_, _, old_blocks) = inode_get(image, &superblock, index).ok_or(())?;
            if !store(image, &superblock, index, &existing, &old_blocks) {
                return Err(());
            }
            let end = existing.len();
            return Ok((end - base, end));
        }

        let empty = [0_u32; DIRECT_BLOCKS];
        if !store(image, &superblock, index, bytes, &empty) {
            return Err(());
        }
        Ok((bytes.len(), bytes.len()))
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

/// Reads the whole image into kernel memory and keeps ownership of the disk.
pub fn load() -> bool {
    if !virtio_blk::init() {
        console::write_str("KSFS: no block device\r\n");
        return false;
    }
    let bytes = IMAGE_SECTORS * BLOCK_SIZE;
    let Some((physical, _tag)) = frame_allocator::alloc_contiguous_pages_tagged(bytes / 4096) else {
        console::write_str("KSFS: image allocation failed\r\n");
        return false;
    };
    let base = phys_to_virt(physical);
    let mut sector = [0_u8; BLOCK_SIZE];
    for index in 0..IMAGE_SECTORS {
        if !virtio_blk::read_sector(index as u64, &mut sector) {
            console::write_str("KSFS: image read failed\r\n");
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
    unsafe {
        IMAGE_PTR = base as *mut u8;
        IMAGE_LEN = bytes;
    }
    // Refuse an image whose metadata cannot be trusted instead of mounting a
    // filesystem where every operation is guaranteed to fail.
    let snapshot = unsafe { core::slice::from_raw_parts(IMAGE_PTR, IMAGE_LEN) };
    if superblock(snapshot).is_err() {
        unsafe {
            IMAGE_PTR = core::ptr::null_mut();
            IMAGE_LEN = 0;
        }
        console::write_str("KSFS: bad superblock\r\n");
        return false;
    }
    true
}

/// Diagnostic summary printed once at boot.
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
