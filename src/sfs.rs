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
//! Scope of this step: hierarchical directories of any depth, four direct
//! blocks plus one single indirect block per file (66 KiB), and the usual
//! create/truncate/append, mkdir, unlink, rmdir and rename operations. A
//! directory holds up to 64 entries (four direct blocks); double-indirect
//! files and directory growth are next.

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
/// Pointers in the single indirect block (512 / 4). A file is therefore capped
/// at 4 * 512 + 128 * 512 = 66 KiB, enough for an app ELF.
const INDIRECT_POINTERS: usize = BLOCK_SIZE / 4;
const MAX_FILE_BLOCKS: usize = DIRECT_BLOCKS + INDIRECT_POINTERS;

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
) -> Option<(u32, u32, [u32; DIRECT_BLOCKS], u32)> {
    let offset = inode_offset_checked(image, superblock, index)?;
    let kind = u32_at(image, offset);
    let size = u32_at(image, offset + 4);
    let mut blocks = [0_u32; DIRECT_BLOCKS];
    for (slot, block) in blocks.iter_mut().enumerate() {
        *block = u32_at(image, offset + 8 + slot * 4);
    }
    // Bytes 24..28 were reserved in format 1 and are zero in existing images,
    // so an older filesystem simply has no indirect block.
    let indirect = u32_at(image, offset + 8 + DIRECT_BLOCKS * 4);
    Some((kind, size, blocks, indirect))
}

fn inode_put(
    image: &mut [u8],
    superblock: &Superblock,
    index: u32,
    kind: u32,
    size: u32,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
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
    put_u32(image, offset + 8 + DIRECT_BLOCKS * 4, indirect);
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

/// Resolves a logical file block to a physical block number, following the
/// single indirect table past the four direct slots.
fn inode_slot_block(
    image: &[u8],
    superblock: &Superblock,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
    slot: usize,
) -> Option<u32> {
    if slot < DIRECT_BLOCKS {
        let block = blocks[slot];
        return (block != 0).then_some(block);
    }
    let index = slot - DIRECT_BLOCKS;
    if index >= INDIRECT_POINTERS || indirect == 0 {
        return None;
    }
    let start = block_start(image, superblock, indirect)?;
    let block = u32_at(image, start + index * 4);
    (block != 0).then_some(block)
}

/// Releases every block an inode owns: the direct entries, the blocks listed
/// in the indirect table, and the table block itself.
fn free_inode_blocks(
    image: &mut [u8],
    superblock: &Superblock,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
) {
    free_blocks(image, superblock, blocks);
    if indirect == 0 {
        return;
    }
    if let Some(start) = block_start(image, superblock, indirect) {
        for index in 0..INDIRECT_POINTERS {
            let block = u32_at(image, start + index * 4);
            if block != 0 {
                free_blocks(image, superblock, &[block]);
            }
        }
    }
    free_blocks(image, superblock, &[indirect]);
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

fn free_blocks(image: &mut [u8], superblock: &Superblock, blocks: &[u32]) {
    for &block in blocks {
        // Never let a corrupt inode release metadata into the free pool.
        if block >= superblock.data_block_start && block < superblock.total_blocks {
            bitmap_set(image, superblock, block, false);
        }
    }
}

/// Finds a root-directory entry by name: `(inode, kind, size)`.
/// Splits a backend-relative path into validated components.
fn split_path(path: &str) -> Result<Vec<&str>, ()> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.len() > NAME_MAX {
            return Err(());
        }
        parts.push(part);
    }
    Ok(parts)
}

/// Resolves a directory path to its inode, rejecting a missing component or a
/// file where a directory is required.
fn resolve_dir(image: &[u8], superblock: &Superblock, parts: &[&str]) -> Option<u32> {
    let mut inode = superblock.root_inode;
    for name in parts {
        let (block, slot) = find_entry(image, superblock, inode, name)?;
        let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
        let (kind, _, _, _) = inode_get(image, superblock, index)?;
        if kind != INODE_KIND_DIR {
            return None;
        }
        inode = index;
    }
    Some(inode)
}

/// Resolves a full path to `(inode, kind, size)`.
fn lookup(image: &[u8], superblock: &Superblock, path: &str) -> Option<(u32, u32, u32)> {
    let parts = split_path(path).ok()?;
    let (&name, parent) = parts.split_last()?;
    let dir = resolve_dir(image, superblock, parent)?;
    let (block, slot) = find_entry(image, superblock, dir, name)?;
    let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
    let (kind, size, _, _) = inode_get(image, superblock, index)?;
    Some((index, kind, size))
}

/// Locates the 32-byte directory slot holding `name` inside `dir`:
/// `(block, slot)`.
fn find_entry(
    image: &[u8],
    superblock: &Superblock,
    dir: u32,
    name: &str,
) -> Option<(usize, usize)> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, blocks, _) = inode_get(image, superblock, dir)?;
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

/// Adds `name -> child` to directory `dir`, allocating a directory block when
/// every existing slot is taken.
fn add_entry(
    image: &mut [u8],
    superblock: &Superblock,
    dir: u32,
    name: &str,
    child: u32,
) -> Option<()> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, mut blocks, _) = inode_get(image, superblock, dir)?;

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
    if !inode_put(image, superblock, dir, INODE_KIND_DIR, 0, &blocks, 0) {
        return None;
    }
    flush_bitmap(image, superblock);
    Some(())
}

fn create_inode(
    image: &mut [u8],
    superblock: &Superblock,
    parent: u32,
    name: &str,
    kind: u32,
) -> Option<u32> {
    let index = find_free_inode(image, superblock)?;
    let empty = [0_u32; DIRECT_BLOCKS];
    if !inode_put(image, superblock, index, kind, 0, &empty, 0) {
        return None;
    }
    if add_entry(image, superblock, parent, name, index).is_none() {
        let _ = inode_put(image, superblock, index, INODE_KIND_FREE, 0, &empty, 0);
        return None;
    }
    flush_bitmap(image, superblock);
    Some(index)
}

fn truncate(image: &mut [u8], superblock: &Superblock, index: u32) -> bool {
    let Some((kind, _, blocks, indirect)) = inode_get(image, superblock, index) else {
        return false;
    };
    let empty = [0_u32; DIRECT_BLOCKS];
    // Publish the empty inode first, then release: a crash in between leaks
    // blocks instead of leaving the inode pointing at blocks another file
    // may already own.
    let ok = inode_put(image, superblock, index, kind, 0, &empty, 0);
    free_inode_blocks(image, superblock, &blocks, indirect);
    flush_bitmap(image, superblock);
    ok
}

fn store(
    image: &mut [u8],
    superblock: &Superblock,
    index: u32,
    bytes: &[u8],
    old_blocks: &[u32; DIRECT_BLOCKS],
    old_indirect: u32,
) -> bool {
    let blocks_needed = bytes.len().div_ceil(BLOCK_SIZE);
    if blocks_needed > MAX_FILE_BLOCKS {
        return false;
    }

    let mut blocks = [0_u32; DIRECT_BLOCKS];
    let mut indirect = 0_u32;
    if blocks_needed > DIRECT_BLOCKS {
        let Some(table) = alloc_block(image, superblock) else {
            return false;
        };
        indirect = table;
        if let Some(start) = block_start(image, superblock, table) {
            // A recycled block may still hold another file's pointers.
            for byte in &mut image[start..start + BLOCK_SIZE] {
                *byte = 0;
            }
        } else {
            free_blocks(image, superblock, &[table]);
            return false;
        }
    }

    for slot in 0..blocks_needed {
        let Some(block) = alloc_block(image, superblock) else {
            free_blocks(image, superblock, &blocks);
            if indirect != 0 {
                free_blocks(image, superblock, &[indirect]);
            }
            return false;
        };
        if slot < DIRECT_BLOCKS {
            blocks[slot] = block;
        } else if let Some(start) = block_start(image, superblock, indirect) {
            put_u32(image, start + (slot - DIRECT_BLOCKS) * 4, block);
        } else {
            return false;
        }
    }
    // Persist the allocation *before* the inode can point at these blocks.
    // A crash after this point leaks blocks (recoverable); the reverse order
    // would let the allocator hand an inode-owned block to another file and
    // cross-link their data.
    if !flush_bitmap(image, superblock) {
        return false;
    }
    if indirect != 0 && !flush_block(image, superblock, indirect as usize) {
        return false;
    }

    for slot in 0..blocks_needed {
        let Some(block) = inode_slot_block(image, superblock, &blocks, indirect, slot) else {
            return false;
        };
        let start = slot * BLOCK_SIZE;
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

    if !inode_put(
        image,
        superblock,
        index,
        INODE_KIND_FILE,
        bytes.len() as u32,
        &blocks,
        indirect,
    ) {
        return false;
    }

    // The inode no longer references the old blocks, so they can be released.
    // Build the new set first so a block that is somehow reused is not freed.
    let mut new_blocks = [0_u32; MAX_FILE_BLOCKS];
    for slot in 0..blocks_needed {
        new_blocks[slot] = inode_slot_block(image, superblock, &blocks, indirect, slot).unwrap_or(0);
    }
    for &block in old_blocks.iter() {
        if block != 0 && !new_blocks.contains(&block) {
            free_blocks(image, superblock, &[block]);
        }
    }
    if old_indirect != 0 && old_indirect != indirect {
        if let Some(start) = block_start(image, superblock, old_indirect) {
            for pointer in 0..INDIRECT_POINTERS {
                let block = u32_at(image, start + pointer * 4);
                if block != 0 && !new_blocks.contains(&block) {
                    free_blocks(image, superblock, &[block]);
                }
            }
        }
        if !new_blocks.contains(&old_indirect) {
            free_blocks(image, superblock, &[old_indirect]);
        }
    }
    flush_bitmap(image, superblock);
    true
}

pub struct Sfs;

impl FileSystem for Sfs {
    fn create_file(&mut self, path: &str) -> Result<(), ()> {
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        match find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
                let (kind, _, _, _) = inode_get(image, &superblock, index).ok_or(())?;
                if kind != INODE_KIND_FILE {
                    return Err(());
                }
                if truncate(image, &superblock, index) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            None => create_inode(image, &superblock, parent, name, INODE_KIND_FILE)
                .map(|_| ())
                .ok_or(()),
        }
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        if find_entry(image, &superblock, parent, name).is_some() {
            return Err(());
        }
        create_inode(image, &superblock, parent, name, INODE_KIND_DIR)
            .map(|_| ())
            .ok_or(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), ()> {
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let (block, slot) = find_entry(image, &superblock, parent, name).ok_or(())?;
        let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
        let (kind, _, blocks, indirect) = inode_get(image, &superblock, index).ok_or(())?;
        if kind != INODE_KIND_FILE {
            return Err(());
        }
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        if !flush_block(image, &superblock, block) {
            return Err(());
        }
        let empty = [0_u32; DIRECT_BLOCKS];
        inode_put(image, &superblock, index, INODE_KIND_FREE, 0, &empty, 0);
        // Release only after the inode stopped referencing the blocks: a crash
        // in between leaks them instead of cross-linking two files.
        free_inode_blocks(image, &superblock, &blocks, indirect);
        flush_bitmap(image, &superblock);
        Ok(())
    }

    fn remove_directory(&mut self, path: &str) -> Result<(), ()> {
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let (block, slot) = find_entry(image, &superblock, parent, name).ok_or(())?;
        let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
        let (kind, _, blocks, indirect) = inode_get(image, &superblock, index).ok_or(())?;
        if kind != INODE_KIND_DIR
            || indirect != 0
            || blocks.iter().any(|&block| block != 0)
        {
            return Err(());
        }
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        if !flush_block(image, &superblock, block) {
            return Err(());
        }
        let empty = [0_u32; DIRECT_BLOCKS];
        inode_put(image, &superblock, index, INODE_KIND_FREE, 0, &empty, 0);
        flush_bitmap(image, &superblock);
        Ok(())
    }

    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let source_parts = split_path(source)?;
        let dest_parts = split_path(destination)?;
        let (&old_name, old_parent_parts) = source_parts.split_last().ok_or(())?;
        let (&new_name, new_parent_parts) = dest_parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let old_parent = resolve_dir(image, &superblock, old_parent_parts).ok_or(())?;
        let new_parent = resolve_dir(image, &superblock, new_parent_parts).ok_or(())?;
        if find_entry(image, &superblock, new_parent, new_name).is_some() {
            return Err(());
        }
        let (block, slot) = find_entry(image, &superblock, old_parent, old_name).ok_or(())?;
        let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);

        if old_parent == new_parent {
            let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
            image[entry + 4] = new_name.len() as u8;
            image[entry + 5..entry + 5 + new_name.len()].copy_from_slice(new_name.as_bytes());
            for byte in &mut image[entry + 5 + new_name.len()..entry + DIR_ENTRY_SIZE] {
                *byte = 0;
            }
            if !flush_block(image, &superblock, block) {
                return Err(());
            }
            return Ok(());
        }

        add_entry(image, &superblock, new_parent, new_name, index).ok_or(())?;
        let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
        for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
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
        let parts = split_path(path)?;
        if parts.is_empty() {
            return Ok(true);
        }
        let (_, kind, _) = lookup(image, &superblock, path).ok_or(())?;
        Ok(kind == INODE_KIND_DIR)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        let parts = split_path(path)?;
        let dir = resolve_dir(image, &superblock, &parts).ok_or(())?;
        let (_, _, blocks, _) = inode_get(image, &superblock, dir).ok_or(())?;
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
                let (kind, _, _, _) = inode_get(image, &superblock, index).ok_or(())?;
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
        let (index, _, size) = lookup(image, &superblock, path).ok_or(())?;
        if offset >= size as usize {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(limit).min(size as usize);
        let (_, _, blocks, indirect) = inode_get(image, &superblock, index).ok_or(())?;
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let slot = position / BLOCK_SIZE;
            let block = inode_slot_block(image, &superblock, &blocks, indirect, slot).ok_or(())?;
            let start = block_start(image, &superblock, block).ok_or(())?;
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
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let index = match find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
                let (kind, _, _, _) = inode_get(image, &superblock, index).ok_or(())?;
                if kind != INODE_KIND_FILE {
                    return Err(());
                }
                index
            }
            None => create_inode(image, &superblock, parent, name, INODE_KIND_FILE).ok_or(())?,
        };

        let base = if append {
            let (_, _, size) = lookup(image, &superblock, path).ok_or(())?;
            size as usize
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
            let (_, _, old_blocks, old_indirect) = inode_get(image, &superblock, index).ok_or(())?;
            let mut existing = Vec::new();
            let mut position = 0;
            while position < base {
                let slot = position / BLOCK_SIZE;
                let block =
                    inode_slot_block(image, &superblock, &old_blocks, old_indirect, slot)
                        .ok_or(())?;
                let start = block_start(image, &superblock, block).ok_or(())?;
                let within = position % BLOCK_SIZE;
                let take = (BLOCK_SIZE - within).min(base - position);
                existing.extend_from_slice(&image[start + within..start + within + take]);
                position += take;
            }
            existing.extend_from_slice(bytes);
            if !store(image, &superblock, index, &existing, &old_blocks, old_indirect) {
                return Err(());
            }
            let end = existing.len();
            return Ok((end - base, end));
        }

        let empty = [0_u32; DIRECT_BLOCKS];
        if !store(image, &superblock, index, bytes, &empty, 0) {
            return Err(());
        }
        Ok((bytes.len(), bytes.len()))
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let image = image()?;
        let superblock = superblock(image)?;
        if split_path(path)?.is_empty() {
            return Ok(0);
        }
        let (_, kind, size) = lookup(image, &superblock, path).ok_or(())?;
        if kind != INODE_KIND_FILE {
            return Err(());
        }
        Ok(size as usize)
    }

    fn seed_file(&mut self, path: &str, contents: &[u8]) -> Result<(), ()> {
        if contents.len() > MAX_FILE_BLOCKS * BLOCK_SIZE {
            return Err(());
        }
        let parts = split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = superblock(image)?;
        let parent = resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let index = match find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
                let (kind, _, _, _) = inode_get(image, &superblock, index).ok_or(())?;
                if kind != INODE_KIND_FILE {
                    return Err(());
                }
                index
            }
            None => create_inode(image, &superblock, parent, name, INODE_KIND_FILE).ok_or(())?,
        };
        if !truncate(image, &superblock, index) {
            return Err(());
        }
        let empty = [0_u32; DIRECT_BLOCKS];
        if !store(image, &superblock, index, contents, &empty, 0) {
            return Err(());
        }
        flush_bitmap(image, &superblock);
        Ok(())
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
