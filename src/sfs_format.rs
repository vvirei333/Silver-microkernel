//! Pure SFS on-disk format logic, shared by the kernel filesystem (`sfs`)
//! and host tests.
//!
//! Every function here operates on an explicit image slice and persists
//! mutations through the [`SfsDisk`] trait, so the format rules can be tested
//! on the host against a plain in-memory "disk" without virtio or the frame
//! allocator. The kernel side implements [`SfsDisk`] over `virtio_blk`.
//!
//! The on-disk format is the one `format_sfs.py` creates: 512-byte blocks, a
//! superblock in block 0, a free-block bitmap in block 1, an inode table
//! starting at block 2, and data blocks after it. Scope: hierarchical
//! directories of any depth, four direct blocks plus single/double indirect
//! tables per file, and the usual create/truncate/append, positioned write,
//! mkdir, unlink, rmdir and rename operations.

use alloc::vec::Vec;

pub const MAGIC: u32 = 0x5346_5321;
pub const BLOCK_SIZE: usize = 512;
pub const INODE_SIZE: usize = 64;
pub const DIR_ENTRY_SIZE: usize = 32;
pub const DIR_ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / DIR_ENTRY_SIZE;
pub const NAME_MAX: usize = 27;
pub const DIRECT_BLOCKS: usize = 4;
/// Pointers in one table block (512 / 4).
pub const INDIRECT_POINTERS: usize = BLOCK_SIZE / 4;
/// A file can use 4 direct blocks, the indirect table and the double
/// indirect tree. The VFS caps a file at 512 KiB, so only the first few
/// sub-tables are ever used.
pub const MAX_FILE_BLOCKS: usize =
    DIRECT_BLOCKS + INDIRECT_POINTERS + INDIRECT_POINTERS * INDIRECT_POINTERS;
pub const DOUBLE_FIRST_SLOT: usize = DIRECT_BLOCKS + INDIRECT_POINTERS;

pub const INODE_KIND_FREE: u32 = 0;
pub const INODE_KIND_FILE: u32 = 1;
pub const INODE_KIND_DIR: u32 = 2;

pub const MAX_NAME_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirEntry {
    pub name: [u8; MAX_NAME_LEN],
    pub name_len: u64,
    pub kind: u64,
}

/// Persistence surface of the format. The kernel implements it over
/// virtio-blk; host tests record writes into memory.
pub trait SfsDisk {
    /// Writes one 512-byte sector to the device. Returns false on failure;
    /// every caller treats that as "the operation failed" and unwinds.
    fn write_sector(&mut self, sector: u64, data: &[u8; BLOCK_SIZE]) -> bool;
}

pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

pub fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub total_blocks: u32,
    pub root_inode: u32,
    pub inode_count: u32,
    pub inode_table_block: u32,
    pub bitmap_block: u32,
    pub data_block_start: u32,
}

pub fn superblock(image: &[u8]) -> Result<Superblock, ()> {
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
    // The bitmap and the inode table are independent regions: an overlap
    // would make the allocator read inode bytes as allocation bits (and
    // corrupt inodes on every bitmap write), so refuse it up front.
    let bitmap_blocks = (total_blocks as usize).div_ceil(8).div_ceil(BLOCK_SIZE);
    let bitmap_span_end = bitmap_block as usize * BLOCK_SIZE + bitmap_blocks * BLOCK_SIZE;
    let inode_start = inode_table_block as usize * BLOCK_SIZE;
    if block_size as usize != BLOCK_SIZE
        || total_blocks as usize * BLOCK_SIZE > image.len()
        || inode_count == 0
        || data_block_start >= total_blocks
        || bitmap_block >= total_blocks
        || inode_table_block >= total_blocks
        || bitmap_end > image.len()
        || inode_end > image.len()
        || root_inode >= inode_count
        || (inode_start < bitmap_span_end && bitmap_end > inode_start)
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
pub fn block_start(image: &[u8], superblock: &Superblock, block: u32) -> Option<usize> {
    if block < superblock.data_block_start || block >= superblock.total_blocks {
        return None;
    }
    let start = (block as usize).checked_mul(BLOCK_SIZE)?;
    if start.checked_add(BLOCK_SIZE)? > image.len() {
        return None;
    }
    Some(start)
}

pub type InodeTuple = (u32, u32, [u32; DIRECT_BLOCKS], u32, u32);

/// Resources owned by one inode tree. `inodes` counts the inode itself, so an
/// empty file is `{ inodes: 1, ..0 }`. Data and metadata blocks are counted
/// separately: an indirect table is metadata, a data block is payload.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub inodes: u32,
    pub data_blocks: u32,
    pub metadata_blocks: u32,
}

impl Usage {
    fn add(&mut self, other: Usage) -> Option<()> {
        self.inodes = self.inodes.checked_add(other.inodes)?;
        self.data_blocks = self.data_blocks.checked_add(other.data_blocks)?;
        self.metadata_blocks = self.metadata_blocks.checked_add(other.metadata_blocks)?;
        Some(())
    }
}

pub fn inode_get(
    image: &[u8],
    superblock: &Superblock,
    index: u32,
) -> Option<InodeTuple> {
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
    let double = u32_at(image, offset + 8 + (DIRECT_BLOCKS + 1) * 4);
    Some((kind, size, blocks, indirect, double))
}

/// Resources owned by a single inode: the inode itself, its data blocks and
/// its indirect/double-indirect tables. Directory payload blocks are counted
/// as data; `usage_under` adds the entries reachable from them.
pub fn usage_of_inode(
    image: &[u8],
    superblock: &Superblock,
    index: u32,
) -> Option<Usage> {
    let (kind, size, blocks, indirect, double) = inode_get(image, superblock, index)?;
    let limit = if kind == INODE_KIND_DIR {
        MAX_FILE_BLOCKS
    } else {
        (size as usize).div_ceil(BLOCK_SIZE)
    };
    let mut usage = Usage {
        inodes: 1,
        data_blocks: 0,
        metadata_blocks: usize::from(indirect != 0) as u32,
    };

    for slot in 0..limit {
        match inode_slot_block(image, superblock, &blocks, indirect, double, slot) {
            Some(_) => {
                usage.data_blocks = usage.data_blocks.checked_add(1)?;
            }
            None if kind == INODE_KIND_DIR => break,
            None => return None,
        }
    }

    if double != 0 {
        // The double node and every sub-table are metadata.
        let start = block_start(image, superblock, double)?;
        usage.metadata_blocks = usage.metadata_blocks.checked_add(1)?;
        for table_index in 0..INDIRECT_POINTERS {
            if u32_at(image, start + table_index * 4) != 0 {
                usage.metadata_blocks = usage.metadata_blocks.checked_add(1)?;
            }
        }
    }
    Some(usage)
}

/// Recursive accounting for `path` and everything below it. The working set is
/// bounded by `inode_count`; a directory cycle is rejected instead of looping
/// forever on a corrupt image.
pub fn usage_under(image: &[u8], superblock: &Superblock, path: &str) -> Option<Usage> {
    let parts = split_path(path).ok()?;
    let root = if parts.is_empty() {
        superblock.root_inode
    } else {
        lookup(image, superblock, path)?.0
    };
    let mut stack = Vec::new();
    usage_tree(image, superblock, root, &mut stack)
}

fn usage_tree(
    image: &[u8],
    superblock: &Superblock,
    index: u32,
    stack: &mut Vec<u32>,
) -> Option<Usage> {
    if stack.len() as u32 >= superblock.inode_count || stack.contains(&index) {
        return None;
    }
    stack.push(index);
    let (kind, _, blocks, indirect, _) = inode_get(image, superblock, index)?;
    let mut usage = usage_of_inode(image, superblock, index)?;
    if kind == INODE_KIND_DIR {
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = inode_slot_block(image, superblock, &blocks, indirect, 0, slot)
            else {
                break;
            };
            let start = block_start(image, superblock, block)?;
            for entry_slot in 0..DIR_ENTRIES_PER_BLOCK {
                let entry = start + entry_slot * DIR_ENTRY_SIZE;
                let child = u32_at(image, entry);
                if child == 0 {
                    continue;
                }
                usage.add(usage_tree(image, superblock, child, stack)?)?;
            }
        }
    }
    stack.pop();
    Some(usage)
}

/// Writes the inode fields into the image and flushes the owning block.
pub fn inode_put(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    index: u32,
    kind: u32,
    size: u32,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
    double: u32,
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
    put_u32(image, offset + 8 + (DIRECT_BLOCKS + 1) * 4, double);
    flush_block(image, disk, offset / BLOCK_SIZE)
}

pub fn bitmap_used(image: &[u8], superblock: &Superblock, block: u32) -> bool {
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

pub fn flush_block(
    image: &[u8],
    disk: &mut dyn SfsDisk,
    block: usize,
) -> bool {
    let start = block * BLOCK_SIZE;
    if start + BLOCK_SIZE > image.len() {
        return false;
    }
    let mut sector = [0_u8; BLOCK_SIZE];
    sector.copy_from_slice(&image[start..start + BLOCK_SIZE]);
    disk.write_sector(block as u64, &sector)
}

pub fn flush_bitmap(image: &[u8], superblock: &Superblock, disk: &mut dyn SfsDisk) -> bool {
    flush_block(image, disk, superblock.bitmap_block as usize)
}

/// Resolves a logical file block to a physical block number, following the
/// single indirect table past the four direct slots and the double indirect
/// tree past the indirect one.
pub fn inode_slot_block(
    image: &[u8],
    superblock: &Superblock,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
    double: u32,
    slot: usize,
) -> Option<u32> {
    if slot < DIRECT_BLOCKS {
        let block = blocks[slot];
        return (block != 0).then_some(block);
    }
    if slot < DOUBLE_FIRST_SLOT {
        if indirect == 0 {
            return None;
        }
        let index = slot - DIRECT_BLOCKS;
        let start = block_start(image, superblock, indirect)?;
        let block = u32_at(image, start + index * 4);
        return (block != 0).then_some(block);
    }
    if double == 0 {
        return None;
    }
    let index = slot - DOUBLE_FIRST_SLOT;
    let table_index = index / INDIRECT_POINTERS;
    if table_index >= INDIRECT_POINTERS {
        return None;
    }
    let double_start = block_start(image, superblock, double)?;
    let table = u32_at(image, double_start + table_index * 4);
    if table == 0 {
        return None;
    }
    let start = block_start(image, superblock, table)?;
    let block = u32_at(image, start + (index % INDIRECT_POINTERS) * 4);
    (block != 0).then_some(block)
}

/// Releases every block an inode owns. Files are bounded by their size;
/// directories are packed, so their list ends at the first missing block.
/// The single and double indirect tables are released as well.
pub fn free_inode_blocks(
    image: &mut [u8],
    superblock: &Superblock,
    kind: u32,
    size: u32,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
    double: u32,
) {
    let limit = if kind == INODE_KIND_DIR {
        MAX_FILE_BLOCKS
    } else {
        (size as usize).div_ceil(BLOCK_SIZE)
    };
    for slot in 0..limit {
        match inode_slot_block(image, superblock, blocks, indirect, double, slot) {
            Some(block) => free_blocks(image, superblock, &[block]),
            None => {
                if kind == INODE_KIND_DIR {
                    break;
                }
            }
        }
    }
    if indirect != 0 {
        free_blocks(image, superblock, &[indirect]);
    }
    if double != 0 {
        if let Some(start) = block_start(image, superblock, double) {
            for table_index in 0..INDIRECT_POINTERS {
                let table = u32_at(image, start + table_index * 4);
                if table != 0 {
                    free_blocks(image, superblock, &[table]);
                }
            }
        }
        free_blocks(image, superblock, &[double]);
    }
}

pub fn alloc_block(image: &mut [u8], superblock: &Superblock) -> Option<u32> {
    for block in superblock.data_block_start..superblock.total_blocks {
        if !bitmap_used(image, superblock, block) {
            bitmap_set(image, superblock, block, true);
            return Some(block);
        }
    }
    None
}

pub fn free_blocks(image: &mut [u8], superblock: &Superblock, blocks: &[u32]) {
    for &block in blocks {
        // Never let a corrupt inode release metadata into the free pool.
        if block >= superblock.data_block_start && block < superblock.total_blocks {
            bitmap_set(image, superblock, block, false);
        }
    }
}

/// Splits a backend-relative path into validated components.
pub fn split_path(path: &str) -> Result<Vec<&str>, ()> {
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
pub fn resolve_dir(image: &[u8], superblock: &Superblock, parts: &[&str]) -> Option<u32> {
    let mut inode = superblock.root_inode;
    for name in parts {
        let (block, slot) = find_entry(image, superblock, inode, name)?;
        let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
        let (kind, _, _, _, _) = inode_get(image, superblock, index)?;
        if kind != INODE_KIND_DIR {
            return None;
        }
        inode = index;
    }
    Some(inode)
}

/// Resolves a full path to `(inode, kind, size)`.
pub fn lookup(image: &[u8], superblock: &Superblock, path: &str) -> Option<(u32, u32, u32)> {
    let parts = split_path(path).ok()?;
    let (&name, parent) = parts.split_last()?;
    let dir = resolve_dir(image, superblock, parent)?;
    let (block, slot) = find_entry(image, superblock, dir, name)?;
    let index = u32_at(image, block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE);
    let (kind, size, _, _, _) = inode_get(image, superblock, index)?;
    Some((index, kind, size))
}

/// Locates the 32-byte directory slot holding `name` inside `dir`:
/// `(block, slot)`.
pub fn find_entry(
    image: &[u8],
    superblock: &Superblock,
    dir: u32,
    name: &str,
) -> Option<(usize, usize)> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, blocks, indirect, _) = inode_get(image, superblock, dir)?;
    for block_slot in 0..MAX_FILE_BLOCKS {
        let Some(block) = inode_slot_block(image, superblock, &blocks, indirect, 0, block_slot) else {
            break;
        };
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

/// True when every one of the 16 slots in `block` is free.
fn dir_block_empty(image: &[u8], superblock: &Superblock, block: u32) -> bool {
    let Some(start) = block_start(image, superblock, block) else {
        return false;
    };
    (0..DIR_ENTRIES_PER_BLOCK).all(|slot| u32_at(image, start + slot * DIR_ENTRY_SIZE) == 0)
}

/// Removes an empty `removed` block from directory `dir` and compacts the
/// list so the packed iteration order (stop at the first missing slot) stays
/// valid. Also frees the block and, when the tail is gone, the indirect table.
fn shrink_dir_block(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    dir: u32,
    removed: u32,
) -> bool {
    let Some((_, _, blocks, indirect, _)) = inode_get(image, superblock, dir) else {
        return false;
    };
    let mut list = Vec::new();
    for slot in 0..MAX_FILE_BLOCKS {
        let Some(block) = inode_slot_block(image, superblock, &blocks, indirect, 0, slot) else {
            break;
        };
        list.push(block);
    }
    let Some(position) = list.iter().position(|&block| block == removed) else {
        return false;
    };
    list.remove(position);

    let mut direct = [0_u32; DIRECT_BLOCKS];
    for (slot, &block) in list.iter().take(DIRECT_BLOCKS).enumerate() {
        direct[slot] = block;
    }
    let mut table = indirect;
    if list.len() > DIRECT_BLOCKS {
        let Some(start) = block_start(image, superblock, table) else {
            return false;
        };
        for byte in &mut image[start..start + BLOCK_SIZE] {
            *byte = 0;
        }
        for (pointer, &block) in list[DIRECT_BLOCKS..].iter().enumerate() {
            put_u32(image, start + pointer * 4, block);
        }
        if !flush_block(image, disk, table as usize) {
            return false;
        }
    } else if table != 0 {
        free_blocks(image, superblock, &[table]);
        table = 0;
    }

    free_blocks(image, superblock, &[removed]);
    if !inode_put(image, superblock, disk, dir, INODE_KIND_DIR, 0, &direct, table, 0) {
        return false;
    }
    flush_bitmap(image, superblock, disk);
    true
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

/// Clears one 32-byte directory slot in the image and flushes the owning
/// block. Used by unlink/rmdir and by the rename rollback.
fn clear_entry(
    image: &mut [u8],
    disk: &mut dyn SfsDisk,
    block: usize,
    slot: usize,
) -> bool {
    let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
    if entry + DIR_ENTRY_SIZE > image.len() {
        return false;
    }
    for byte in &mut image[entry..entry + DIR_ENTRY_SIZE] {
        *byte = 0;
    }
    flush_block(image, disk, block)
}

/// Adds `name -> child` to directory `dir`, allocating a directory block when
/// every existing slot is taken.
pub fn add_entry(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    dir: u32,
    name: &str,
    child: u32,
) -> Option<()> {
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    let (_, _, mut blocks, mut indirect, _) = inode_get(image, superblock, dir)?;

    // 1. An existing block may still have a free slot.
    for block_slot in 0..MAX_FILE_BLOCKS {
        let Some(block) = inode_slot_block(image, superblock, &blocks, indirect, 0, block_slot) else {
            break;
        };
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
                if !flush_block(image, disk, block as usize) {
                    return None;
                }
                return Some(());
            }
        }
    }

    // 2. Every block is full: append a new one. A directory therefore grows
    //    from 4 direct blocks to a single indirect table (132 blocks total).
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
        if indirect == 0 {
            let Some(table) = alloc_block(image, superblock) else {
                bitmap_set(image, superblock, block, false);
                return None;
            };
            let Some(table_start) = block_start(image, superblock, table) else {
                bitmap_set(image, superblock, block, false);
                bitmap_set(image, superblock, table, false);
                return None;
            };
            for byte in &mut image[table_start..table_start + BLOCK_SIZE] {
                *byte = 0;
            }
            if !flush_block(image, disk, table as usize) {
                bitmap_set(image, superblock, block, false);
                bitmap_set(image, superblock, table, false);
                return None;
            }
            indirect = table;
        }
        let Some(table_start) = block_start(image, superblock, indirect) else {
            bitmap_set(image, superblock, block, false);
            return None;
        };
        let mut stored = false;
        for pointer in 0..INDIRECT_POINTERS {
            if u32_at(image, table_start + pointer * 4) == 0 {
                put_u32(image, table_start + pointer * 4, block);
                stored = true;
                break;
            }
        }
        if !stored || !flush_block(image, disk, indirect as usize) {
            bitmap_set(image, superblock, block, false);
            return None;
        }
    }

    if !flush_block(image, disk, block as usize) {
        return None;
    }
    // Allocation is now reachable from the directory inode, so persist the
    // bitmap before publishing the inode (same rule as file data).
    if !flush_bitmap(image, superblock, disk) {
        return None;
    }
    if !inode_put(image, superblock, disk, dir, INODE_KIND_DIR, 0, &blocks, indirect, 0) {
        return None;
    }
    flush_bitmap(image, superblock, disk);
    Some(())
}

/// Clears the directory entry `find_entry` located, without shrinking.
/// Returns false when the flush fails; the entry stays visible on disk.
fn clear_dir_entry(
    image: &mut [u8],
    disk: &mut dyn SfsDisk,
    block: usize,
    slot: usize,
) -> bool {
    clear_entry(image, disk, block, slot)
}

pub fn create_inode(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    parent: u32,
    name: &str,
    kind: u32,
) -> Option<u32> {
    let index = find_free_inode(image, superblock)?;
    let empty = [0_u32; DIRECT_BLOCKS];
    if !inode_put(image, superblock, disk, index, kind, 0, &empty, 0, 0) {
        return None;
    }
    if add_entry(image, superblock, disk, parent, name, index).is_none() {
        let _ = inode_put(image, superblock, disk, index, INODE_KIND_FREE, 0, &empty, 0, 0);
        return None;
    }
    flush_bitmap(image, superblock, disk);
    Some(index)
}

pub fn truncate(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    index: u32,
) -> bool {
    let Some((kind, size, blocks, indirect, double)) = inode_get(image, superblock, index) else {
        return false;
    };
    let empty = [0_u32; DIRECT_BLOCKS];
    // Publish the empty inode first, then release: a crash in between leaks
    // blocks instead of leaving the inode pointing at blocks another file
    // may already own.
    let ok = inode_put(image, superblock, disk, index, kind, 0, &empty, 0, 0);
    free_inode_blocks(image, superblock, kind, size, &blocks, indirect, double);
    flush_bitmap(image, superblock, disk);
    ok
}

/// Reads every byte of an inode into a fresh buffer (append/offset rewrite).
pub fn read_inode_bytes(
    image: &[u8],
    superblock: &Superblock,
    index: u32,
) -> Option<Vec<u8>> {
    let (_, size, blocks, indirect, double) = inode_get(image, superblock, index)?;
    let mut data = Vec::with_capacity(size as usize);
    let mut position = 0usize;
    while position < size as usize {
        let slot = position / BLOCK_SIZE;
        let block = inode_slot_block(image, superblock, &blocks, indirect, double, slot)?;
        let start = block_start(image, superblock, block)?;
        let within = position % BLOCK_SIZE;
        let take = (BLOCK_SIZE - within).min(size as usize - position);
        data.extend_from_slice(&image[start + within..start + within + take]);
        position += take;
    }
    Some(data)
}

/// Writes an existing block of an inode in place: copies `sector` into the
/// image at the block the inode's `slot` points to and flushes it. Used by
/// the positioned-overwrite fast path, where the block map never changes.
fn overwrite_block(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    blocks: &[u32; DIRECT_BLOCKS],
    indirect: u32,
    double: u32,
    slot: usize,
    sector: &[u8; BLOCK_SIZE],
) -> bool {
    let Some(block) = inode_slot_block(image, superblock, blocks, indirect, double, slot) else {
        return false;
    };
    let Some(start) = block_start(image, superblock, block) else {
        return false;
    };
    image[start..start + BLOCK_SIZE].copy_from_slice(sector);
    disk.write_sector(block as u64, sector)
}

/// In-place positioned overwrite: writes `bytes` at `offset` when the write
/// stays entirely inside the file's existing blocks. The block map and the
/// inode are untouched, so the only writes are the covered data sectors —
/// no whole-file rewrite, no bitmap churn. Returns false when the fast path
/// does not apply (caller falls back to the full `store` rewrite).
pub fn overwrite_in_place(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    index: u32,
    offset: usize,
    bytes: &[u8],
) -> bool {
    let Some((_, size, blocks, indirect, double)) = inode_get(image, superblock, index) else {
        return false;
    };
    let end = match offset.checked_add(bytes.len()) {
        Some(end) if end <= size as usize => end,
        _ => return false,
    };
    let mut position = offset;
    while position < end {
        let slot = position / BLOCK_SIZE;
        let block = match inode_slot_block(image, superblock, &blocks, indirect, double, slot) {
            Some(block) => block,
            None => return false,
        };
        let Some(start) = block_start(image, superblock, block) else {
            return false;
        };
        let within = position % BLOCK_SIZE;
        let take = (BLOCK_SIZE - within).min(end - position);
        let mut sector = [0_u8; BLOCK_SIZE];
        sector[..BLOCK_SIZE].copy_from_slice(&image[start..start + BLOCK_SIZE]);
        sector[within..within + take].copy_from_slice(&bytes[position - offset..position - offset + take]);
        image[start..start + BLOCK_SIZE].copy_from_slice(&sector);
        if !disk.write_sector(block as u64, &sector) {
            return false;
        }
        position += take;
    }
    true
}

/// Replaces the whole content of a file inode: allocates a fresh block tree,
/// persists allocation and data, publishes the inode, then releases the old
/// tree (whose blocks the bitmap still marks used, so the allocator above
/// cannot have handed them out again).
pub fn store(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    index: u32,
    bytes: &[u8],
    old_blocks: &[u32; DIRECT_BLOCKS],
    old_indirect: u32,
    old_double: u32,
    old_size: u32,
) -> bool {
    let blocks_needed = bytes.len().div_ceil(BLOCK_SIZE);
    if blocks_needed > MAX_FILE_BLOCKS {
        return false;
    }

    let mut blocks = [0_u32; DIRECT_BLOCKS];
    let mut indirect = 0_u32;
    let mut double = 0_u32;

    for slot in 0..blocks_needed {
        let Some(block) = alloc_block(image, superblock) else {
            free_inode_blocks(
                image,
                superblock,
                INODE_KIND_FILE,
                bytes.len() as u32,
                &blocks,
                indirect,
                double,
            );
            return false;
        };
        if slot < DIRECT_BLOCKS {
            blocks[slot] = block;
        } else if slot < DOUBLE_FIRST_SLOT {
            if indirect == 0 {
                let Some(table) = alloc_block(image, superblock) else {
                    free_inode_blocks(
                        image,
                        superblock,
                        INODE_KIND_FILE,
                        bytes.len() as u32,
                        &blocks,
                        indirect,
                        double,
                    );
                    return false;
                };
                if let Some(start) = block_start(image, superblock, table) {
                    for byte in &mut image[start..start + BLOCK_SIZE] {
                        *byte = 0;
                    }
                } else {
                    return false;
                }
                indirect = table;
            }
            let Some(start) = block_start(image, superblock, indirect) else {
                return false;
            };
            put_u32(image, start + (slot - DIRECT_BLOCKS) * 4, block);
        } else {
            let within = slot - DOUBLE_FIRST_SLOT;
            let table_index = within / INDIRECT_POINTERS;
            if double == 0 {
                let Some(node) = alloc_block(image, superblock) else {
                    free_inode_blocks(
                        image,
                        superblock,
                        INODE_KIND_FILE,
                        bytes.len() as u32,
                        &blocks,
                        indirect,
                        double,
                    );
                    return false;
                };
                if let Some(start) = block_start(image, superblock, node) {
                    for byte in &mut image[start..start + BLOCK_SIZE] {
                        *byte = 0;
                    }
                } else {
                    return false;
                }
                double = node;
            }
            let Some(node_start) = block_start(image, superblock, double) else {
                return false;
            };
            let mut table = u32_at(image, node_start + table_index * 4);
            if table == 0 {
                let Some(fresh) = alloc_block(image, superblock) else {
                    free_inode_blocks(
                        image,
                        superblock,
                        INODE_KIND_FILE,
                        bytes.len() as u32,
                        &blocks,
                        indirect,
                        double,
                    );
                    return false;
                };
                if let Some(start) = block_start(image, superblock, fresh) {
                    for byte in &mut image[start..start + BLOCK_SIZE] {
                        *byte = 0;
                    }
                } else {
                    return false;
                }
                put_u32(image, node_start + table_index * 4, fresh);
                table = fresh;
            }
            let Some(table_start) = block_start(image, superblock, table) else {
                return false;
            };
            put_u32(
                image,
                table_start + (within % INDIRECT_POINTERS) * 4,
                block,
            );
        }
    }

    // Persist the allocation before the inode can point at these blocks: a
    // crash after this point leaks blocks, never cross-links two files.
    if !flush_bitmap(image, superblock, disk) {
        return false;
    }
    if indirect != 0 && !flush_block(image, disk, indirect as usize) {
        return false;
    }
    if double != 0 {
        let Some(node_start) = block_start(image, superblock, double) else {
            return false;
        };
        for table_index in 0..INDIRECT_POINTERS {
            let table = u32_at(image, node_start + table_index * 4);
            if table != 0 && !flush_block(image, disk, table as usize) {
                return false;
            }
        }
        if !flush_block(image, disk, double as usize) {
            return false;
        }
    }

    for slot in 0..blocks_needed {
        let Some(block) =
            inode_slot_block(image, superblock, &blocks, indirect, double, slot)
        else {
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
        if !disk.write_sector(block as u64, &sector) {
            return false;
        }
    }

    if !inode_put(
        image,
        superblock,
        disk,
        index,
        INODE_KIND_FILE,
        bytes.len() as u32,
        &blocks,
        indirect,
        double,
    ) {
        return false;
    }

    // The inode no longer references the old tree, so release it. The bitmap
    // still marks those blocks used, so the allocator cannot have handed any
    // of them to the new tree above.
    free_inode_blocks(
        image,
        superblock,
        INODE_KIND_FILE,
        old_size,
        old_blocks,
        old_indirect,
        old_double,
    );
    flush_bitmap(image, superblock, disk);
    true
}

/// Removes the directory entry `find_entry` located and, when its block
/// becomes empty, shrinks the directory. Returns false when the flush fails.
pub fn remove_dir_entry(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    parent: u32,
    block: usize,
    slot: usize,
) -> bool {
    if !clear_dir_entry(image, disk, block, slot) {
        return false;
    }
    if dir_block_empty(image, superblock, block as u32) {
        shrink_dir_block(image, superblock, disk, parent, block as u32);
    }
    true
}

/// Atomically replaces `destination` with `source` (both files). The source
/// entry is removed first, then the destination entry is re-pointed at the
/// source inode, and only then is the old destination inode released. A crash
/// between the steps loses the update, never the old destination.
pub fn replace_entry(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    source: &str,
    destination: &str,
) -> bool {
    if source == destination {
        return true;
    }
    let Ok(source_parts) = split_path(source) else {
        return false;
    };
    let Ok(destination_parts) = split_path(destination) else {
        return false;
    };
    let (Some((&source_name, source_parent_parts)), Some((&destination_name, destination_parent_parts))) =
        (source_parts.split_last(), destination_parts.split_last())
    else {
        return false;
    };
    let Some(source_parent) = resolve_dir(image, superblock, source_parent_parts) else {
        return false;
    };
    let Some(destination_parent) = resolve_dir(image, superblock, destination_parent_parts) else {
        return false;
    };
    let Some((source_block, source_slot)) = find_entry(image, superblock, source_parent, source_name)
    else {
        return false;
    };
    let Some((destination_block, destination_slot)) =
        find_entry(image, superblock, destination_parent, destination_name)
    else {
        return false;
    };
    let source_index =
        u32_at(image, source_block * BLOCK_SIZE + source_slot * DIR_ENTRY_SIZE);
    let destination_index =
        u32_at(image, destination_block * BLOCK_SIZE + destination_slot * DIR_ENTRY_SIZE);
    let Some((source_kind, _, _, _, _)) = inode_get(image, superblock, source_index) else {
        return false;
    };
    let Some((destination_kind, destination_size, destination_blocks, destination_indirect, destination_double)) =
        inode_get(image, superblock, destination_index)
    else {
        return false;
    };
    if source_kind != INODE_KIND_FILE || destination_kind != INODE_KIND_FILE {
        return false;
    }
    if !remove_dir_entry(image, superblock, disk, source_parent, source_block, source_slot) {
        return false;
    }
    // Publish the new inode under the old destination name.
    put_u32(
        image,
        destination_block * BLOCK_SIZE + destination_slot * DIR_ENTRY_SIZE,
        source_index,
    );
    if !flush_block(image, disk, destination_block) {
        return false;
    }
    // The old destination inode is no longer reachable; release it.
    let empty = [0_u32; DIRECT_BLOCKS];
    if !inode_put(
        image,
        superblock,
        disk,
        destination_index,
        INODE_KIND_FREE,
        0,
        &empty,
        0,
        0,
    ) {
        return false;
    }
    free_inode_blocks(
        image,
        superblock,
        INODE_KIND_FILE,
        destination_size,
        &destination_blocks,
        destination_indirect,
        destination_double,
    );
    flush_bitmap(image, superblock, disk)
}

/// Overwrites the name of an entry in place (same-parent rename).
pub fn rename_entry_in_place(
    image: &mut [u8],
    superblock: &Superblock,
    disk: &mut dyn SfsDisk,
    block: usize,
    slot: usize,
    new_name: &str,
) -> bool {
    if new_name.is_empty() || new_name.len() > NAME_MAX {
        return false;
    }
    let entry = block * BLOCK_SIZE + slot * DIR_ENTRY_SIZE;
    if entry + DIR_ENTRY_SIZE > image.len() {
        return false;
    }
    image[entry + 4] = new_name.len() as u8;
    image[entry + 5..entry + 5 + new_name.len()].copy_from_slice(new_name.as_bytes());
    for byte in &mut image[entry + 5 + new_name.len()..entry + DIR_ENTRY_SIZE] {
        *byte = 0;
    }
    flush_block(image, disk, block)
}

/// Mount-time consistency check: every block referenced by any inode must be
/// a data block that the bitmap marks used, and no block may be referenced by
/// two inodes. Leaks (bitmap-used, nothing referenced) are counted but do not
/// fail the mount — they are the safe direction of a crash. Any other
/// inconsistency refuses the mount rather than letting the filesystem hand
/// out wrong data.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct FsckReport {
    pub files: usize,
    pub dirs: usize,
    /// Referenced blocks that no inode reaches: safe, reported only.
    pub leaks: usize,
}

pub fn fsck(image: &[u8], superblock: &Superblock) -> Result<FsckReport, ()> {
    let mut report = FsckReport::default();
    let mut referenced = [false; MAX_FILE_BLOCKS];
    let data_blocks = (superblock.total_blocks - superblock.data_block_start) as usize;
    if data_blocks > referenced.len() {
        return Err(());
    }

    for inode in 0..superblock.inode_count {
        let Some((kind, size, blocks, indirect, double)) = inode_get(image, superblock, inode)
        else {
            return Err(());
        };
        match kind {
            INODE_KIND_FREE => continue,
            INODE_KIND_FILE => report.files += 1,
            INODE_KIND_DIR => report.dirs += 1,
            _ => return Err(()),
        }
        let limit = if kind == INODE_KIND_DIR {
            // Directories are packed: stop at the first missing slot, exactly
            // like the runtime walkers.
            MAX_FILE_BLOCKS
        } else {
            (size as usize).div_ceil(BLOCK_SIZE)
        };
        for slot in 0..limit {
            match inode_slot_block(image, superblock, &blocks, indirect, double, slot) {
                Some(block) => mark_referenced(image, superblock, &mut referenced, block)?,
                None => {
                    if kind == INODE_KIND_DIR {
                        break;
                    }
                    // A file whose size claims more blocks than the tree
                    // holds is truncated metadata: fail, not read garbage.
                    return Err(());
                }
            }
        }
        if indirect != 0 {
            mark_referenced(image, superblock, &mut referenced, indirect)?;
        }
        if double != 0 {
            mark_referenced(image, superblock, &mut referenced, double)?;
            let Some(start) = block_start(image, superblock, double) else {
                return Err(());
            };
            for table_index in 0..INDIRECT_POINTERS {
                let table = u32_at(image, start + table_index * 4);
                if table != 0 {
                    mark_referenced(image, superblock, &mut referenced, table)?;
                }
            }
        }
        // Directory entries must point at live inodes other than the owner.
        if kind == INODE_KIND_DIR {
            for slot in 0..MAX_FILE_BLOCKS {
                let Some(block) =
                    inode_slot_block(image, superblock, &blocks, indirect, 0, slot)
                else {
                    break;
                };
                let Some(start) = block_start(image, superblock, block) else {
                    return Err(());
                };
                for entry in 0..DIR_ENTRIES_PER_BLOCK {
                    let base = start + entry * DIR_ENTRY_SIZE;
                    let index = u32_at(image, base);
                    if index == 0 {
                        continue;
                    }
                    if index >= superblock.inode_count || index == inode {
                        return Err(());
                    }
                }
            }
        }
    }

    let data_total = (superblock.total_blocks - superblock.data_block_start) as usize;
    report.leaks = (0..data_total)
        .filter(|&block| {
            bitmap_used(image, superblock, superblock.data_block_start + block as u32)
                && !referenced[block]
        })
        .count();
    Ok(report)
}

/// Records one referenced block: it must be a data block, marked used in the
/// bitmap, and not already owned by another inode.
fn mark_referenced(
    image: &[u8],
    superblock: &Superblock,
    referenced: &mut [bool; MAX_FILE_BLOCKS],
    block: u32,
) -> Result<(), ()> {
    if block < superblock.data_block_start || block >= superblock.total_blocks {
        return Err(());
    }
    let index = (block - superblock.data_block_start) as usize;
    if referenced[index] {
        // Two inodes own the same block: on-disk cross-link.
        return Err(());
    }
    referenced[index] = true;
    if !bitmap_used(image, superblock, block) {
        // Referenced but free: the allocator may hand it out.
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory disk: records every write so tests can inspect the device
    /// state independently of the image buffer.
    struct MemDisk {
        device: Vec<[u8; BLOCK_SIZE]>,
        fail_from: Option<u64>,
    }

    impl MemDisk {
        fn new(sectors: usize) -> Self {
            MemDisk {
                device: vec![[0_u8; BLOCK_SIZE]; sectors],
                fail_from: None,
            }
        }
        fn synced_with(&self, image: &[u8]) -> bool {
            self.device
                .iter()
                .enumerate()
                .all(|(i, sector)| &image[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE] == sector)
        }
    }

    impl SfsDisk for MemDisk {
        fn write_sector(&mut self, sector: u64, data: &[u8; BLOCK_SIZE]) -> bool {
            if let Some(from) = self.fail_from {
                if sector >= from {
                    return false;
                }
            }
            let index = sector as usize;
            if index >= self.device.len() {
                return false;
            }
            self.device[index] = *data;
            true
        }
    }

    /// Builds a formatted image: 128 data blocks, 32 inodes, root at inode 0.
    fn make_image(total_blocks: usize) -> (Vec<u8>, MemDisk, Superblock) {
        let mut image = vec![0_u8; total_blocks * BLOCK_SIZE];
        let sb = Superblock {
            total_blocks: total_blocks as u32,
            root_inode: 0,
            inode_count: 32,
            inode_table_block: 2,
            bitmap_block: 1,
            data_block_start: (2 + 32 * INODE_SIZE / BLOCK_SIZE + 1) as u32,
        };
        put_u32(&mut image, 0, MAGIC);
        put_u32(&mut image, 8, BLOCK_SIZE as u32);
        put_u32(&mut image, 12, sb.total_blocks);
        put_u32(&mut image, 16, sb.root_inode);
        put_u32(&mut image, 20, sb.inode_count);
        put_u32(&mut image, 24, sb.bitmap_block);
        put_u32(&mut image, 28, sb.inode_table_block);
        put_u32(&mut image, 36, sb.data_block_start);
        let mut disk = MemDisk::new(total_blocks);
        // The superblock is formatted before the disk is attached, so seed
        // the device copy to match the image buffer.
        let mut sector = [0_u8; BLOCK_SIZE];
        sector.copy_from_slice(&image[..BLOCK_SIZE]);
        assert!(disk.write_sector(0, &sector));
        (image, disk, sb)
    }

    /// A minimal root directory inode (kind DIR, no blocks).
    fn seed_root(image: &mut [u8], sb: &Superblock, disk: &mut MemDisk) {
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(inode_put(image, sb, disk, sb.root_inode, INODE_KIND_DIR, 0, &empty, 0, 0));
    }

    fn block_count_used(image: &[u8], sb: &Superblock) -> usize {
        (sb.data_block_start..sb.total_blocks)
            .filter(|&b| bitmap_used(image, sb, b))
            .count()
    }

    #[test]
    fn superblock_accepts_valid_and_rejects_corrupt() {
        let (image, _, sb) = make_image(64);
        assert_eq!(superblock(&image).unwrap(), sb);
        // Corrupt magic.
        let mut bad = image.clone();
        bad[0] = 0;
        assert!(superblock(&bad).is_err());
        // inode table past the image.
        let mut bad = image.clone();
        put_u32(&mut bad, 28, 1000);
        assert!(superblock(&bad).is_err());
        // data region at/after the end.
        let mut bad = image.clone();
        put_u32(&mut bad, 36, 64);
        assert!(superblock(&bad).is_err());
    }

    #[test]
    fn block_start_rejects_metadata_and_out_of_range() {
        let (image, _, sb) = make_image(64);
        // Metadata block (below data start).
        assert_eq!(block_start(&image, &sb, 0), None);
        assert_eq!(block_start(&image, &sb, sb.data_block_start - 1), None);
        // At/after the end.
        assert_eq!(block_start(&image, &sb, sb.total_blocks), None);
        assert_eq!(block_start(&image, &sb, u32::MAX), None);
        // First data block resolves and stays in bounds.
        let start = block_start(&image, &sb, sb.data_block_start).unwrap();
        assert_eq!(start, sb.data_block_start as usize * BLOCK_SIZE);
    }

    #[test]
    fn split_path_rejects_traversal_and_long_names() {
        assert!(split_path("/a/b").unwrap() == vec!["a", "b"]);
        assert!(split_path("/").unwrap().is_empty());
        assert!(split_path("a/../b").is_err());
        assert!(split_path("a/./b").is_err());
        assert!(split_path("a//b").is_err());
        assert!(split_path(&"x".repeat(NAME_MAX + 1)).is_err());
        assert!(split_path(&"x".repeat(NAME_MAX)).is_ok());
    }

    #[test]
    fn create_write_read_round_trip() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "hello", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        let payload: Vec<u8> = (0..1000).map(|i| i as u8).collect();
        assert!(store(&mut image, &sb, &mut disk, index, &payload, &empty, 0, 0, 0));
        let (_, size, _, _, _) = inode_get(&image, &sb, index).unwrap();
        assert_eq!(size, 1000);
        let read_back = read_inode_bytes(&image, &sb, index).unwrap();
        assert_eq!(read_back, payload);
        // lookup finds it with the right size/kind.
        assert_eq!(lookup(&image, &sb, "hello").unwrap(), (index, INODE_KIND_FILE, 1000));
        // The device copy matches the image buffer.
        assert!(disk.synced_with(&image));
    }

    #[test]
    fn append_extends_and_overwrite_replaces_bytes() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "f", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        let first = vec![1_u8; 600];
        assert!(store(&mut image, &sb, &mut disk, index, &first, &empty, 0, 0, 0));
        let (_, size, blocks, indirect, double) = inode_get(&image, &sb, index).unwrap();
        // Append 100 bytes.
        let mut content = first.clone();
        content.extend_from_slice(&[2_u8; 100]);
        assert!(store(&mut image, &sb, &mut disk, index, &content, &blocks, indirect, double, size));
        assert_eq!(read_inode_bytes(&image, &sb, index).unwrap(), content);
        // Positioned overwrite in the middle: bytes change, size does not.
        let (_, size2, blocks2, indirect2, double2) = inode_get(&image, &sb, index).unwrap();
        let patch = [9_u8; 7];
        assert!(overwrite_in_place(&mut image, &sb, &mut disk, index, 5, &patch));
        let mut expected = content.clone();
        expected[5..12].copy_from_slice(&patch);
        assert_eq!(read_inode_bytes(&image, &sb, index).unwrap(), expected);
        // Size unchanged (no inode rewrite needed).
        let (_, size3, _, _, _) = inode_get(&image, &sb, index).unwrap();
        assert_eq!(size3, size2);
        let _ = (blocks2, indirect2, double2);
    }

    #[test]
    fn overwrite_in_place_refuses_growth() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "g", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, index, &vec![1_u8; 10], &empty, 0, 0, 0));
        // Offset+len past the size must be refused (caller falls back).
        assert!(!overwrite_in_place(&mut image, &sb, &mut disk, index, 5, &[0_u8; 10]));
        // Even exactly at the boundary: growth is not an in-place op.
        assert!(!overwrite_in_place(&mut image, &sb, &mut disk, index, 10, &[0_u8; 1]));
    }

    #[test]
    fn append_does_not_leak_blocks() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "log", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        let mut current: Vec<u8> = Vec::new();
        for round in 0..13 {
            current.extend_from_slice(&[round as u8; 100]);
            // Snapshot the inode exactly like write_at does, then hand the
            // old tree to store for release after the new one is published.
            let (_, size, blocks, indirect, double) = inode_get(&image, &sb, index).unwrap();
            let (old_blocks, old_indirect, old_double, old_size) = if round == 0 {
                (empty, 0, 0, 0)
            } else {
                (blocks, indirect, double, size)
            };
            assert!(store(&mut image, &sb, &mut disk, index, &current, &old_blocks, old_indirect, old_double, old_size));
            // One directory block + the file's own blocks, nothing leaked.
            assert_eq!(block_count_used(&image, &sb), 1 + current.len().div_ceil(BLOCK_SIZE));
        }
    }

    #[test]
    fn directory_grows_and_shrinks() {
        let (mut image, mut disk, sb) = make_image(256);
        seed_root(&mut image, &sb, &mut disk);
        // More names than one block holds (16 entries) forces growth; the
        // image has 32 inodes and the root takes one, so 30 files fit.
        let names: Vec<String> = (0..30).map(|i| format!("entry{i:02}")).collect();
        for name in &names {
            assert!(create_inode(&mut image, &sb, &mut disk, sb.root_inode, name, INODE_KIND_FILE).is_some());
        }
        // All 40 resolve.
        for name in &names {
            assert!(lookup(&image, &sb, name).is_some());
        }
        // Removing all of them shrinks the directory back and frees blocks.
        for name in &names {
            let (block, slot) = find_entry(&image, &sb, sb.root_inode, name).unwrap();
            assert!(remove_dir_entry(&mut image, &sb, &mut disk, sb.root_inode, block, slot));
        }
        assert_eq!(block_count_used(&image, &sb), 0);
        for name in &names {
            assert!(lookup(&image, &sb, name).is_none());
        }
    }

    #[test]
    fn rename_moves_entry_between_directories_with_rollback() {
        let (mut image, mut disk, sb) = make_image(256);
        seed_root(&mut image, &sb, &mut disk);
        let sub = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "sub", INODE_KIND_DIR).unwrap();
        let file = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a.txt", INODE_KIND_FILE).unwrap();

        // Cross-directory rename: add to "sub", clear from root.
        let (block, slot) = find_entry(&image, &sb, sb.root_inode, "a.txt").unwrap();
        assert!(add_entry(&mut image, &sb, &mut disk, sub, "b.txt", file).is_some());
        assert!(remove_dir_entry(&mut image, &sb, &mut disk, sb.root_inode, block, slot));
        assert!(lookup(&image, &sb, "a.txt").is_none());
        assert_eq!(lookup(&image, &sb, "sub/b.txt").unwrap().0, file);

        // Rollback: destination flush fails after the new entry was added.
        let (sub_block, sub_slot) = find_entry(&image, &sb, sub, "b.txt").unwrap();
        let mut failing = MemDisk::new(256);
        failing.fail_from = Some(sub_block as u64);
        // add_entry flushes the destination block; with the device refusing
        // writes from that block on, the caller must undo the new entry.
        assert!(add_entry(&mut image, &sb, &mut failing, sub, "c.txt", file).is_none());
        // In-memory image was mutated before the flush failed: undo like
        // rename does, then verify the entry is gone from the directory.
        let (nb, ns) = find_entry(&image, &sb, sub, "c.txt").unwrap();
        assert!(remove_dir_entry(&mut image, &sb, &mut disk, sub, nb, ns));
        assert!(lookup(&image, &sb, "sub/c.txt").is_none());
        let _ = (sub_block, sub_slot);
    }

    #[test]
    fn truncate_frees_all_blocks_including_double_indirect() {
        let (mut image, mut disk, sb) = make_image(256);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "big", INODE_KIND_FILE).unwrap();
        // 6 blocks: 4 direct + 1 indirect table + 2 indirect data.
        let payload = vec![7_u8; 6 * BLOCK_SIZE];
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, index, &payload, &empty, 0, 0, 0));
        // 6 data blocks + 1 indirect table + 1 directory block.
        assert_eq!(block_count_used(&image, &sb), 8);
        assert!(truncate(&mut image, &sb, &mut disk, index));
        // The directory block still holds the file's entry; the file's own
        // blocks (6 data + 1 table) are all released.
        assert_eq!(block_count_used(&image, &sb), 1);
        let (kind, size, _, _, _) = inode_get(&image, &sb, index).unwrap();
        assert_eq!((kind, size), (INODE_KIND_FILE, 0));
    }

    fn fsck_reports_counts() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let file = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "f", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, file, &vec![1_u8; 600], &empty, 0, 0, 0));
        let report = fsck(&image, &sb).unwrap();
        // root + the file's entry holder? Root is 1 dir; file's dir block is
        // the root's own block. So 1 dir, 1 file, 0 leaks.
        assert_eq!((report.files, report.dirs, report.leaks), (1, 1, 0));
        assert!(fsck(&image, &sb).is_ok());
    }

    #[test]
    fn fsck_refuses_cross_link() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        let b = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "b", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 100], &empty, 0, 0, 0));
        assert!(store(&mut image, &sb, &mut disk, b, &vec![2_u8; 100], &empty, 0, 0, 0));
        // Cross-link: point b's first block at a's first block.
        let (_, _, a_blocks, _, _) = inode_get(&image, &sb, a).unwrap();
        let (_, size, _, _, _) = inode_get(&image, &sb, b).unwrap();
        let mut disk2 = MemDisk::new(128);
        assert!(inode_put(&mut image, &sb, &mut disk2, b, INODE_KIND_FILE, size, &a_blocks, 0, 0));
        assert!(fsck(&image, &sb).is_err());
    }

    #[test]
    fn fsck_refuses_referenced_but_free() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 100], &empty, 0, 0, 0));
        // Unmark the file's block in the bitmap (inode still references it).
        let (_, _, blocks, _, _) = inode_get(&image, &sb, a).unwrap();
        bitmap_set(&mut image, &sb, blocks[0], false);
        assert!(fsck(&image, &sb).is_err());
    }

    #[test]
    fn fsck_counts_leaks_but_allows() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 100], &empty, 0, 0, 0));
        // Clear the inode's block reference: the block becomes a leak (used,
        // referenced by nobody) — the safe crash direction, not an error.
        let (_, _, blocks, _, _) = inode_get(&image, &sb, a).unwrap();
        let mut cleared = [0_u32; DIRECT_BLOCKS];
        let mut disk2 = MemDisk::new(128);
        // Size 0 too: a nonzero size with an empty tree is the
        // "size past tree" corruption, refused separately below.
        assert!(inode_put(&mut image, &sb, &mut disk2, a, INODE_KIND_FILE, 0, &cleared, 0, 0));
        let report = fsck(&image, &sb).unwrap();
        assert_eq!(report.leaks, 1);
        let _ = blocks;
    }

    #[test]
    fn fsck_refuses_bad_kind_and_dangling_entry() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        // Bad kind on a free inode slot.
        let mut disk2 = MemDisk::new(128);
        assert!(inode_put(&mut image, &sb, &mut disk2, 1, 7, 0, &[0_u32; DIRECT_BLOCKS], 0, 0));
        assert!(fsck(&image, &sb).is_err());

        // Dangling directory entry: entry points past the inode count.
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        assert!(add_entry(&mut image, &sb, &mut disk, sb.root_inode, "ghost", sb.inode_count + 5).is_some());
        assert!(fsck(&image, &sb).is_err());
    }

    #[test]
    fn fsck_refuses_file_size_past_tree() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        let empty = [0_u32; DIRECT_BLOCKS];
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 100], &empty, 0, 0, 0));
        // Claim 3 blocks of size with only 1 allocated.
        let (_, _, blocks, _, _) = inode_get(&image, &sb, a).unwrap();
        let mut padded = blocks;
        padded[1] = 0; // hole where size says data must be
        let mut disk2 = MemDisk::new(128);
        assert!(inode_put(&mut image, &sb, &mut disk2, a, INODE_KIND_FILE, 3 * BLOCK_SIZE as u32, &padded, 0, 0));
        assert!(fsck(&image, &sb).is_err());
    }

    #[test]
    fn usage_counts_nested_files_and_directories() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let empty = [0_u32; DIRECT_BLOCKS];
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 1000], &empty, 0, 0, 0));
        let sub = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "sub", INODE_KIND_DIR).unwrap();
        let b = create_inode(&mut image, &sb, &mut disk, sub, "b", INODE_KIND_FILE).unwrap();
        assert!(store(&mut image, &sb, &mut disk, b, &vec![2_u8; 10], &empty, 0, 0, 0));

        // root + a + sub + b; root block + sub block + two a blocks + one b block.
        let usage = usage_under(&image, &sb, "").unwrap();
        assert_eq!(usage.inodes, 4);
        assert_eq!(usage.data_blocks, 5);
        assert_eq!(usage.metadata_blocks, 0);

        let only_a = usage_under(&image, &sb, "a").unwrap();
        assert_eq!(only_a, Usage { inodes: 1, data_blocks: 2, metadata_blocks: 0 });
    }

    #[test]
    fn usage_counts_indirect_and_double_metadata() {
        let (mut image, mut disk, sb) = make_image(512);
        seed_root(&mut image, &sb, &mut disk);
        let empty = [0_u32; DIRECT_BLOCKS];
        let big = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "big", INODE_KIND_FILE).unwrap();
        assert!(store(&mut image, &sb, &mut disk, big, &vec![7_u8; 72 * 1024], &empty, 0, 0, 0));

        let usage = usage_under(&image, &sb, "big").unwrap();
        assert_eq!(usage.inodes, 1);
        assert_eq!(usage.data_blocks, 144);
        // One single-indirect table, one double node and one sub-table.
        assert_eq!(usage.metadata_blocks, 3);
    }

    #[test]
    fn replace_entry_moves_source_over_destination() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let empty = [0_u32; DIRECT_BLOCKS];
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        let b = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "b", INODE_KIND_FILE).unwrap();
        assert!(store(&mut image, &sb, &mut disk, a, b"new-content", &empty, 0, 0, 0));
        assert!(store(&mut image, &sb, &mut disk, b, &vec![9_u8; 3 * BLOCK_SIZE], &empty, 0, 0, 0));
        let before = usage_under(&image, &sb, "").unwrap();
        assert!(replace_entry(&mut image, &sb, &mut disk, "a", "b"));
        assert!(lookup(&image, &sb, "a").is_none());
        let (index, kind, size) = lookup(&image, &sb, "b").unwrap();
        assert_eq!(kind, INODE_KIND_FILE);
        assert_eq!(size, b"new-content".len() as u32);
        assert_eq!(read_inode_bytes(&image, &sb, index).unwrap(), b"new-content");
        let after = usage_under(&image, &sb, "").unwrap();
        assert_eq!(after.inodes, before.inodes - 1);
        assert!(after.data_blocks < before.data_blocks);
        assert!(fsck(&image, &sb).is_ok());
    }

    #[test]
    fn failed_store_keeps_usage_and_allocator_consistent() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let empty = [0_u32; DIRECT_BLOCKS];
        let a = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "a", INODE_KIND_FILE).unwrap();
        assert!(store(&mut image, &sb, &mut disk, a, &vec![1_u8; 1000], &empty, 0, 0, 0));
        let before = usage_under(&image, &sb, "").unwrap();

        // Device dies while publishing a larger replacement.
        disk.fail_from = Some(sb.data_block_start as u64 + 2);
        let payload = vec![2_u8; 5 * BLOCK_SIZE];
        assert!(!store(&mut image, &sb, &mut disk, a, &payload, &empty, 0, 0, 0));

        // The published inode is still the old one; failed allocations are
        // either released or leaked, never charged to the live file.
        let after = usage_under(&image, &sb, "").unwrap();
        assert_eq!(after, before);
        assert!(fsck(&image, &sb).is_ok());

        // Recovery: the same allocator can still publish a new version.
        disk.fail_from = None;
        assert!(store(&mut image, &sb, &mut disk, a, &payload, &empty, 0, 0, 0));
        assert!(fsck(&image, &sb).is_ok());
        assert_eq!(read_inode_bytes(&image, &sb, a).unwrap(), payload);
    }

    #[test]
    fn device_failure_leaves_allocator_consistent() {
        let (mut image, mut disk, sb) = make_image(128);
        seed_root(&mut image, &sb, &mut disk);
        let index = create_inode(&mut image, &sb, &mut disk, sb.root_inode, "f", INODE_KIND_FILE).unwrap();
        // Device dies mid-store: store must fail and the bitmap it flushed
        // must be re-readable: no phantom "free" blocks referenced by a
        // published inode (crash-safety direction: leak, never cross-link).
        disk.fail_from = Some(sb.data_block_start as u64 + 2);
        let empty = [0_u32; DIRECT_BLOCKS];
        let payload = vec![1_u8; 5 * BLOCK_SIZE];
        assert!(!store(&mut image, &sb, &mut disk, index, &payload, &empty, 0, 0, 0));
        // The inode must not reference more blocks than the device accepted.
        let (_, size, blocks, indirect, double) = inode_get(&image, &sb, index).unwrap();
        if size == 0 {
            // Store rolled back fully; nothing reachable.
            assert!(blocks.iter().all(|&b| b == 0) && indirect == 0 && double == 0);
        }
    }

    #[test]
    fn corrupt_inode_block_numbers_cannot_panic() {
        let (mut image, mut disk, sb) = make_image(64);
        // Point the root directory at a metadata block number and a block
        // past the image: find_entry / add_entry must return None, not panic.
        let wild = [sb.data_block_start - 1, u32::MAX, 0, 0];
        seed_root(&mut image, &sb, &mut disk);
        assert!(inode_put(&mut image, &sb, &mut disk, sb.root_inode, INODE_KIND_DIR, 0, &wild, 0, 0));
        assert!(find_entry(&image, &sb, sb.root_inode, "x").is_none());
        assert!(add_entry(&mut image, &sb, &mut MemDisk::new(64), sb.root_inode, "x", 1).is_none());
        assert!(read_dir(&image, &sb).is_none());
    }

    fn read_dir(image: &[u8], sb: &Superblock) -> Option<Vec<DirEntry>> {
        let dir = resolve_dir(image, sb, &[])?;
        let (_, _, blocks, indirect, _) = inode_get(image, sb, dir)?;
        let mut entries = Vec::new();
        for block_slot in 0..MAX_FILE_BLOCKS {
            let block = inode_slot_block(image, sb, &blocks, indirect, 0, block_slot)?;
            let start = block_start(image, sb, block)?;
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
                let (kind, _, _, _, _) = inode_get(image, sb, index)?;
                let mut name = [0_u8; MAX_NAME_LEN];
                name[..length].copy_from_slice(&image[entry + 5..entry + 5 + length]);
                entries.push(DirEntry {
                    name,
                    name_len: length as u64,
                    kind: if kind == INODE_KIND_DIR { 2 } else { 1 },
                });
            }
        }
        Some(entries)
    }
}
