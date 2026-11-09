//! Kernel SFS: a streaming, read-only VFS backend.
//!
//! The kernel owns the virtio-blk device and reads 512-byte blocks on demand
//! through a small write-through cache; the whole image is no longer loaded
//! into RAM. Metadata (superblock, inodes, directories, indirect tables) and
//! file data all come from the same block cache, so the RAM footprint is
//! bounded by [`CACHE_BLOCKS`] instead of the image size.
//!
//! SFS mutations (create/unlink/rename/write) are not available in streaming
//! mode: writes would need to allocate blocks and update the bitmap/inode
//! tables, which is the next format slice. `/tmp` remains writable on RamFs.
//! The offline tools (`scripts/sfs_*.py`) and `sfs_format` host tests still
//! own the write format.

use alloc::vec::Vec;

use crate::console;
use crate::mutex::Mutex;
use crate::sfs_format::{BLOCK_SIZE, DIRECT_BLOCKS, INDIRECT_POINTERS, INODE_SIZE};
use crate::vfs::{DirEntry, FileSystem, FsUsage, MAX_NAME_LEN};
use crate::virtio_blk;

const MAGIC: u32 = 0x5346_5321;
const INODE_KIND_FREE: u32 = 0;
const INODE_KIND_FILE: u32 = 1;
const INODE_KIND_DIR: u32 = 2;
const DIR_ENTRY_SIZE: usize = 32;
const DIR_ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / DIR_ENTRY_SIZE;
const NAME_LIMIT: usize = 27;
const MAX_FILE_BLOCKS: usize =
    DIRECT_BLOCKS + INDIRECT_POINTERS + INDIRECT_POINTERS * INDIRECT_POINTERS;

/// 512 cached blocks = 256 KiB. Enough for the inode table scan and a few
/// directory/file blocks without rereading them on every step.
const CACHE_BLOCKS: usize = 512;

const _: () = assert!(BLOCK_SIZE == 512);

#[derive(Copy, Clone, Debug)]
struct Superblock {
    total_blocks: u32,
    root_inode: u32,
    inode_count: u32,
    inode_table_block: u32,
    bitmap_block: u32,
    data_block_start: u32,
}

#[derive(Copy, Clone)]
struct Inode {
    kind: u32,
    size: u32,
    blocks: [u32; DIRECT_BLOCKS],
    indirect: u32,
    double: u32,
}

impl Inode {
    fn is_dir(&self) -> bool {
        self.kind == INODE_KIND_DIR
    }
    fn is_file(&self) -> bool {
        self.kind == INODE_KIND_FILE
    }
}

struct State {
    superblock: Option<Superblock>,
    device_blocks: u32,
}

static STATE: Mutex<State> = Mutex::new(State {
    superblock: None,
    device_blocks: 0,
});

struct Cache {
    keys: [u32; CACHE_BLOCKS],
    data: [[u8; BLOCK_SIZE]; CACHE_BLOCKS],
    next: usize,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    keys: [u32::MAX; CACHE_BLOCKS],
    data: [[0_u8; BLOCK_SIZE]; CACHE_BLOCKS],
    next: 0,
});

fn put_u32_at(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Reads one block through the cache; the cache is never held across I/O.
fn read_block(block: u32, out: &mut [u8; BLOCK_SIZE]) -> bool {
    {
        let cache = CACHE.lock();
        for index in 0..CACHE_BLOCKS {
            if cache.keys[index] == block {
                out.copy_from_slice(&cache.data[index]);
                return true;
            }
        }
    }
    if !virtio_blk::read_sector(block as u64, out) {
        return false;
    }
    let mut cache = CACHE.lock();
    let index = cache.next;
    cache.next = (index + 1) % CACHE_BLOCKS;
    cache.keys[index] = block;
    cache.data[index].copy_from_slice(out);
    true
}

fn parse_superblock(block: &[u8; BLOCK_SIZE], device_blocks: u32) -> Option<Superblock> {
    if u32_at(block, 0) != MAGIC {
        return None;
    }
    let block_size = u32_at(block, 8);
    let total_blocks = u32_at(block, 12);
    let root_inode = u32_at(block, 16);
    let inode_count = u32_at(block, 20);
    let bitmap_block = u32_at(block, 24);
    let inode_table_block = u32_at(block, 28);
    let data_block_start = u32_at(block, 36);
    if block_size as usize != BLOCK_SIZE
        || total_blocks == 0
        || total_blocks > device_blocks
        || inode_count == 0
        || root_inode >= inode_count
        || bitmap_block >= total_blocks
        || inode_table_block >= total_blocks
        || data_block_start >= total_blocks
    {
        return None;
    }
    let bitmap_blocks = (total_blocks as usize)
        .div_ceil(8)
        .div_ceil(BLOCK_SIZE) as u32;
    let inode_blocks = ((inode_count as usize) * INODE_SIZE).div_ceil(BLOCK_SIZE) as u32;
    if bitmap_block.checked_add(bitmap_blocks)? > total_blocks
        || inode_table_block.checked_add(inode_blocks)? > total_blocks
    {
        return None;
    }
    Some(Superblock {
        total_blocks,
        root_inode,
        inode_count,
        inode_table_block,
        bitmap_block,
        data_block_start,
    })
}

/// Writes one block through to the device and updates the cache.
fn write_block(block: u32, data: &[u8; BLOCK_SIZE]) -> bool {
    if !virtio_blk::write_sector(block as u64, data) {
        return false;
    }
    let mut cache = CACHE.lock();
    for index in 0..CACHE_BLOCKS {
        if cache.keys[index] == block {
            cache.data[index].copy_from_slice(data);
            return true;
        }
    }
    let index = cache.next;
    cache.next = (index + 1) % CACHE_BLOCKS;
    cache.keys[index] = block;
    cache.data[index].copy_from_slice(data);
    true
}

fn zero_block(block: u32) -> bool {
    write_block(block, &[0_u8; BLOCK_SIZE])
}

fn split_path(path: &str) -> Option<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts.pop()?;
    Some((parts, name))
}

fn read_status_bits(block: u32, used: bool) -> Option<[u8; BLOCK_SIZE]> {
    let mut bytes = [0_u8; BLOCK_SIZE];
    if !read_block(block, &mut bytes) {
        return None;
    }
    let _ = used;
    Some(bytes)
}

fn reader() -> Option<Reader> {
    let state = STATE.lock();
    Some(Reader {
        superblock: state.superblock?,
        device_blocks: state.device_blocks,
    })
}

#[derive(Copy, Clone)]
struct Reader {
    superblock: Superblock,
    device_blocks: u32,
}

impl Reader {
    fn inode(&self, index: u32) -> Option<Inode> {
        if index >= self.superblock.inode_count {
            return None;
        }
        let offset = (index as usize) * INODE_SIZE;
        let block = self.superblock.inode_table_block + (offset / BLOCK_SIZE) as u32;
        let within = offset % BLOCK_SIZE;
        if within + INODE_SIZE > BLOCK_SIZE {
            return None;
        }
        let mut bytes = [0_u8; BLOCK_SIZE];
        if !read_block(block, &mut bytes) {
            return None;
        }
        Some(Inode {
            kind: u32_at(&bytes, within),
            size: u32_at(&bytes, within + 4),
            blocks: [
                u32_at(&bytes, within + 8),
                u32_at(&bytes, within + 12),
                u32_at(&bytes, within + 16),
                u32_at(&bytes, within + 20),
            ],
            indirect: u32_at(&bytes, within + 24),
            double: u32_at(&bytes, within + 28),
        })
    }

    fn valid_data_block(&self, block: u32) -> bool {
        block >= self.superblock.data_block_start && block < self.superblock.total_blocks
    }

    /// Physical block backing logical `slot`, or `None` for a hole/EOF.
    fn block_at(&self, inode: &Inode, slot: usize) -> Option<u32> {
        let block = if slot < DIRECT_BLOCKS {
            inode.blocks[slot]
        } else if slot < DIRECT_BLOCKS + INDIRECT_POINTERS {
            if inode.indirect == 0 {
                return None;
            }
            let mut table = [0_u8; BLOCK_SIZE];
            if !read_block(inode.indirect, &mut table) {
                return None;
            }
            u32_at(&table, (slot - DIRECT_BLOCKS) * 4)
        } else {
            let position = slot - DIRECT_BLOCKS - INDIRECT_POINTERS;
            if position >= INDIRECT_POINTERS * INDIRECT_POINTERS || inode.double == 0 {
                return None;
            }
            let table_index = position / INDIRECT_POINTERS;
            let pointer = position % INDIRECT_POINTERS;
            let mut node = [0_u8; BLOCK_SIZE];
            if !read_block(inode.double, &mut node) {
                return None;
            }
            let table_block = u32_at(&node, table_index * 4);
            if table_block == 0 {
                return None;
            }
            let mut table = [0_u8; BLOCK_SIZE];
            if !read_block(table_block, &mut table) {
                return None;
            }
            u32_at(&table, pointer * 4)
        };
        if block == 0 || !self.valid_data_block(block) {
            None
        } else {
            Some(block)
        }
    }

    fn resolve(&self, path: &str) -> Option<u32> {
        let mut current = self.superblock.root_inode;
        for part in path.split('/') {
            if part.is_empty() {
                continue;
            }
            current = self.find_child(current, part)?;
        }
        Some(current)
    }

    fn find_child(&self, directory: u32, name: &str) -> Option<u32> {
        let inode = self.inode(directory)?;
        if !inode.is_dir() {
            return None;
        }
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = self.block_at(&inode, slot) else {
                break;
            };
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for entry in 0..DIR_ENTRIES_PER_BLOCK {
                let offset = entry * DIR_ENTRY_SIZE;
                let child = u32_at(&bytes, offset);
                if child == 0 {
                    continue;
                }
                let length = bytes[offset + 4] as usize;
                if length == 0 || length > NAME_LIMIT {
                    continue;
                }
                if &bytes[offset + 5..offset + 5 + length] == name.as_bytes() {
                    return Some(child);
                }
            }
        }
        None
    }

    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Option<Vec<u8>> {
        let index = self.resolve(path)?;
        let inode = self.inode(index)?;
        if !inode.is_file() {
            return None;
        }
        let size = inode.size as usize;
        if offset >= size {
            return Some(Vec::new());
        }
        let end = offset.saturating_add(limit).min(size);
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let slot = position / BLOCK_SIZE;
            let block = self.block_at(&inode, slot)?;
            let within = position % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(end - position);
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            data.extend_from_slice(&bytes[within..within + take]);
            position += take;
        }
        Some(data)
    }

    fn entries(&self, directory: u32) -> Option<Vec<DirEntry>> {
        let inode = self.inode(directory)?;
        if !inode.is_dir() {
            return None;
        }
        let mut entries = Vec::new();
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = self.block_at(&inode, slot) else {
                break;
            };
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for entry in 0..DIR_ENTRIES_PER_BLOCK {
                let offset = entry * DIR_ENTRY_SIZE;
                let index = u32_at(&bytes, offset);
                if index == 0 {
                    continue;
                }
                let length = bytes[offset + 4] as usize;
                if length == 0 || length > NAME_LIMIT {
                    continue;
                }
                let child = self.inode(index)?;
                let mut name = [0_u8; MAX_NAME_LEN];
                name[..length].copy_from_slice(&bytes[offset + 5..offset + 5 + length]);
                entries.push(DirEntry {
                    name,
                    name_len: length as u64,
                    kind: if child.is_dir() { 2 } else { 1 },
                });
            }
        }
        Some(entries)
    }

    fn usage(&self) -> Option<FsUsage> {
        let superblock = self.superblock;
        let bitmap_blocks = (superblock.total_blocks as usize)
            .div_ceil(8)
            .div_ceil(BLOCK_SIZE) as u32;
        let mut used_blocks = 0u32;
        for index in 0..bitmap_blocks {
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(superblock.bitmap_block + index, &mut bytes) {
                return None;
            }
            for (byte_index, byte) in bytes.iter().enumerate() {
                let bit_base = ((index as usize * BLOCK_SIZE + byte_index) * 8) as u32;
                for bit in 0..8 {
                    if bit_base + bit >= superblock.total_blocks {
                        break;
                    }
                    if byte & (1 << bit) != 0 {
                        used_blocks += 1;
                    }
                }
            }
        }
        let inode_blocks = ((superblock.inode_count as usize) * INODE_SIZE)
            .div_ceil(BLOCK_SIZE) as u32;
        let mut used_inodes = 0u32;
        for index in 0..inode_blocks {
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(superblock.inode_table_block + index, &mut bytes) {
                return None;
            }
            for slot in 0..(BLOCK_SIZE / INODE_SIZE) {
                let offset = slot * INODE_SIZE;
                let inode_index = index as usize * (BLOCK_SIZE / INODE_SIZE) + slot;
                if inode_index >= superblock.inode_count as usize {
                    break;
                }
                if u32_at(&bytes, offset) != INODE_KIND_FREE {
                    used_inodes += 1;
                }
            }
        }
        Some(FsUsage {
            inodes: used_inodes,
            data_blocks: used_blocks,
            metadata_blocks: 0,
        })
    }

    fn is_directory(&self, path: &str) -> bool {
        self.resolve(path)
            .and_then(|index| self.inode(index))
            .map(|inode| inode.is_dir())
            .unwrap_or(false)
    }

    fn len(&self, path: &str) -> Option<usize> {
        self.resolve(path)
            .and_then(|index| self.inode(index))
            .map(|inode| inode.size as usize)
    }

    fn resolve_parts(&self, parts: &[&str]) -> Option<u32> {
        let mut current = self.superblock.root_inode;
        for part in parts {
            current = self.find_child(current, part)?;
        }
        Some(current)
    }

    fn write_inode(&self, index: u32, inode: Inode) -> bool {
        if index >= self.superblock.inode_count {
            return false;
        }
        let offset = (index as usize) * INODE_SIZE;
        let block = self.superblock.inode_table_block + (offset / BLOCK_SIZE) as u32;
        let within = offset % BLOCK_SIZE;
        let mut bytes = [0_u8; BLOCK_SIZE];
        if !read_block(block, &mut bytes) {
            return false;
        }
        put_u32_at(&mut bytes, within, inode.kind);
        put_u32_at(&mut bytes, within + 4, inode.size);
        for (slot, value) in inode.blocks.iter().enumerate() {
            put_u32_at(&mut bytes, within + 8 + slot * 4, *value);
        }
        put_u32_at(&mut bytes, within + 24, inode.indirect);
        put_u32_at(&mut bytes, within + 28, inode.double);
        write_block(block, &bytes)
    }

    fn allocate_block(&self) -> Option<u32> {
        let total = self.superblock.total_blocks;
        let bitmap_blocks = (total as usize).div_ceil(8).div_ceil(BLOCK_SIZE) as u32;
        for index in 0..bitmap_blocks {
            let block = self.superblock.bitmap_block + index;
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for byte in 0..BLOCK_SIZE {
                if bytes[byte] == 0xff {
                    continue;
                }
                for bit in 0..8 {
                    let candidate =
                        ((index as usize * BLOCK_SIZE + byte) * 8 + bit) as u32;
                    if candidate >= self.superblock.data_block_start
                        && candidate < total
                        && bytes[byte] & (1 << bit) == 0
                    {
                        bytes[byte] |= 1 << bit;
                        if !write_block(block, &bytes) {
                            return None;
                        }
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    fn free_block(&self, block: u32) -> bool {
        if block < self.superblock.data_block_start || block >= self.superblock.total_blocks {
            return false;
        }
        let offset = (block as usize) / 8;
        let bitmap_block = self.superblock.bitmap_block + (offset / BLOCK_SIZE) as u32;
        let byte = offset % BLOCK_SIZE;
        let mut bytes = [0_u8; BLOCK_SIZE];
        if !read_block(bitmap_block, &mut bytes) {
            return false;
        }
        bytes[byte] &= !(1 << (block % 8));
        write_block(bitmap_block, &bytes)
    }

    fn allocate_inode(&self, kind: u32) -> Option<u32> {
        let inode_blocks = ((self.superblock.inode_count as usize) * INODE_SIZE)
            .div_ceil(BLOCK_SIZE) as u32;
        for index in 0..inode_blocks {
            let block = self.superblock.inode_table_block + index;
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for slot in 0..(BLOCK_SIZE / INODE_SIZE) {
                let offset = slot * INODE_SIZE;
                let candidate = index as usize * (BLOCK_SIZE / INODE_SIZE) + slot;
                if candidate >= self.superblock.inode_count as usize {
                    break;
                }
                if u32_at(&bytes, offset) == INODE_KIND_FREE {
                    let inode = Inode {
                        kind,
                        size: 0,
                        blocks: [0; DIRECT_BLOCKS],
                        indirect: 0,
                        double: 0,
                    };
                    if !self.write_inode(candidate as u32, inode) {
                        return None;
                    }
                    return Some(candidate as u32);
                }
            }
        }
        None
    }

    fn install_block(&self, inode: &mut Inode, slot: usize, block: u32) -> bool {
        if slot < DIRECT_BLOCKS {
            if inode.blocks[slot] != 0 {
                return false;
            }
            inode.blocks[slot] = block;
            return true;
        }
        if slot < DIRECT_BLOCKS + INDIRECT_POINTERS {
            if inode.indirect == 0 {
                let Some(table) = self.allocate_block() else {
                    return false;
                };
                if !zero_block(table) {
                    let _ = self.free_block(table);
                    return false;
                }
                inode.indirect = table;
            }
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(inode.indirect, &mut bytes) {
                return false;
            }
            put_u32_at(&mut bytes, (slot - DIRECT_BLOCKS) * 4, block);
            return write_block(inode.indirect, &bytes);
        }
        let position = slot - DIRECT_BLOCKS - INDIRECT_POINTERS;
        if position >= INDIRECT_POINTERS * INDIRECT_POINTERS {
            return false;
        }
        let table_index = position / INDIRECT_POINTERS;
        let pointer = position % INDIRECT_POINTERS;
        if inode.double == 0 {
            let Some(table) = self.allocate_block() else {
                return false;
            };
            if !zero_block(table) {
                let _ = self.free_block(table);
                return false;
            }
            inode.double = table;
        }
        let mut node = [0_u8; BLOCK_SIZE];
        if !read_block(inode.double, &mut node) {
            return false;
        }
        let mut table = u32_at(&node, table_index * 4);
        if table == 0 {
            let Some(allocated) = self.allocate_block() else {
                return false;
            };
            if !zero_block(allocated) {
                let _ = self.free_block(allocated);
                return false;
            }
            put_u32_at(&mut node, table_index * 4, allocated);
            if !write_block(inode.double, &node) {
                return false;
            }
            table = allocated;
        }
        let mut bytes = [0_u8; BLOCK_SIZE];
        if !read_block(table, &mut bytes) {
            return false;
        }
        put_u32_at(&mut bytes, pointer * 4, block);
        write_block(table, &bytes)
    }

    fn truncate_inode(&self, index: u32, mut inode: Inode) -> bool {
        let mut slot = 0;
        while let Some(block) = self.block_at(&inode, slot) {
            let _ = self.free_block(block);
            slot += 1;
            if slot >= MAX_FILE_BLOCKS {
                break;
            }
        }
        if inode.indirect != 0 {
            let _ = self.free_block(inode.indirect);
        }
        if inode.double != 0 {
            let mut node = [0_u8; BLOCK_SIZE];
            if read_block(inode.double, &mut node) {
                for table_index in 0..INDIRECT_POINTERS {
                    let table = u32_at(&node, table_index * 4);
                    if table != 0 {
                        let _ = self.free_block(table);
                    }
                }
            }
            let _ = self.free_block(inode.double);
        }
        inode.size = 0;
        inode.blocks = [0; DIRECT_BLOCKS];
        inode.indirect = 0;
        inode.double = 0;
        self.write_inode(index, inode)
    }

    fn dir_add(&self, parent: u32, name: &str, child: u32) -> bool {
        if name.len() == 0 || name.len() > NAME_LIMIT {
            return false;
        }
        let mut inode = match self.inode(parent) {
            Some(inode) if inode.is_dir() => inode,
            _ => return false,
        };
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = self.block_at(&inode, slot) else {
                break;
            };
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return false;
            }
            for entry in 0..DIR_ENTRIES_PER_BLOCK {
                let offset = entry * DIR_ENTRY_SIZE;
                if u32_at(&bytes, offset) == 0 {
                    put_u32_at(&mut bytes, offset, child);
                    bytes[offset + 4] = name.len() as u8;
                    bytes[offset + 5..offset + 5 + name.len()]
                        .copy_from_slice(name.as_bytes());
                    return write_block(block, &bytes);
                }
            }
        }
        let mut slot = 0;
        while self.block_at(&inode, slot).is_some() {
            slot += 1;
            if slot >= MAX_FILE_BLOCKS {
                return false;
            }
        }
        let Some(block) = self.allocate_block() else {
            return false;
        };
        if !zero_block(block) {
            let _ = self.free_block(block);
            return false;
        }
        let mut bytes = [0_u8; BLOCK_SIZE];
        put_u32_at(&mut bytes, 0, child);
        bytes[4] = name.len() as u8;
        bytes[5..5 + name.len()].copy_from_slice(name.as_bytes());
        if !write_block(block, &bytes) {
            let _ = self.free_block(block);
            return false;
        }
        if !self.install_block(&mut inode, slot, block) {
            let _ = self.free_block(block);
            return false;
        }
        self.write_inode(parent, inode)
    }

    fn dir_remove(&self, parent: u32, name: &str) -> Option<u32> {
        let inode = self.inode(parent)?;
        if !inode.is_dir() {
            return None;
        }
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = self.block_at(&inode, slot) else {
                break;
            };
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for entry in 0..DIR_ENTRIES_PER_BLOCK {
                let offset = entry * DIR_ENTRY_SIZE;
                let child = u32_at(&bytes, offset);
                if child == 0 {
                    continue;
                }
                let length = bytes[offset + 4] as usize;
                if length == 0 || length > NAME_LIMIT {
                    continue;
                }
                if &bytes[offset + 5..offset + 5 + length] == name.as_bytes() {
                    put_u32_at(&mut bytes, offset, 0);
                    for byte in &mut bytes[offset + 4..offset + DIR_ENTRY_SIZE] {
                        *byte = 0;
                    }
                    if !write_block(block, &bytes) {
                        return None;
                    }
                    return Some(child);
                }
            }
        }
        None
    }

    /// Finds a directory entry, returning the block/slot that stores it.
    fn find_entry(&self, parent: u32, name: &str) -> Option<(u32, usize, u32)> {
        let inode = self.inode(parent)?;
        if !inode.is_dir() {
            return None;
        }
        for slot in 0..MAX_FILE_BLOCKS {
            let Some(block) = self.block_at(&inode, slot) else {
                break;
            };
            let mut bytes = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut bytes) {
                return None;
            }
            for entry in 0..DIR_ENTRIES_PER_BLOCK {
                let offset = entry * DIR_ENTRY_SIZE;
                let child = u32_at(&bytes, offset);
                if child == 0 {
                    continue;
                }
                let length = bytes[offset + 4] as usize;
                if length == 0 || length > NAME_LIMIT {
                    continue;
                }
                if &bytes[offset + 5..offset + 5 + length] == name.as_bytes() {
                    return Some((block, entry, child));
                }
            }
        }
        None
    }

    /// Rewrites one directory entry in place: inode pointer plus name.
    fn write_entry(&self, block: u32, slot: usize, child: u32, name: &str) -> bool {
        if name.is_empty() || name.len() > NAME_LIMIT {
            return false;
        }
        let mut bytes = [0_u8; BLOCK_SIZE];
        if !read_block(block, &mut bytes) {
            return false;
        }
        let offset = slot * DIR_ENTRY_SIZE;
        put_u32_at(&mut bytes, offset, child);
        for byte in &mut bytes[offset + 4..offset + DIR_ENTRY_SIZE] {
            *byte = 0;
        }
        bytes[offset + 4] = name.len() as u8;
        bytes[offset + 5..offset + 5 + name.len()].copy_from_slice(name.as_bytes());
        write_block(block, &bytes)
    }

    fn is_under(source: &[&str], destination: &[&str]) -> bool {
        destination.len() > source.len()
            && destination[..source.len()] == *source
    }

    /// Moves an entry without overwriting. Same-parent moves rename in place;
    /// cross-parent moves add the destination first and roll back on failure.
    fn rename(&self, source: &str, destination: &str) -> bool {
        if source == destination {
            return false;
        }
        let Some((source_parents, source_name)) = split_path(source) else {
            return false;
        };
        let Some((destination_parents, destination_name)) = split_path(destination) else {
            return false;
        };
        let Some(source_parent) = self.resolve_parts(&source_parents) else {
            return false;
        };
        let Some(destination_parent) = self.resolve_parts(&destination_parents) else {
            return false;
        };
        let Some((source_block, source_slot, child)) = self.find_entry(source_parent, source_name)
        else {
            return false;
        };
        if self.find_entry(destination_parent, destination_name).is_some() {
            return false;
        }
        let Some(inode) = self.inode(child) else {
            return false;
        };
        // A directory can never move into its own subtree. Compare full paths,
        // not just the parents: every "/x" has empty parents, so parent-only
        // comparison would refuse every root-level directory move.
        let mut source_parts = source_parents.clone();
        source_parts.push(source_name);
        let mut destination_parts = destination_parents.clone();
        destination_parts.push(destination_name);
        if inode.is_dir() && Self::is_under(&source_parts, &destination_parts) {
            return false;
        }
        if source_parent == destination_parent {
            return self.write_entry(source_block, source_slot, child, destination_name);
        }
        if !self.dir_add(destination_parent, destination_name, child) {
            return false;
        }
        if !self.dir_remove(source_parent, source_name).is_some() {
            // Undo the destination entry so a failure leaves one name, not two.
            let _ = self.dir_remove(destination_parent, destination_name);
            return false;
        }
        true
    }

    /// Replaces `destination` with `source`. The source entry is detached
    /// first, then the destination name is re-pointed at the source inode;
    /// only then is the old destination inode released. A failure before the
    /// re-point restores the source entry.
    fn replace(&self, source: &str, destination: &str) -> bool {
        if source == destination {
            return false;
        }
        let Some((source_parents, source_name)) = split_path(source) else {
            return false;
        };
        let Some((destination_parents, destination_name)) = split_path(destination) else {
            return false;
        };
        let Some(source_parent) = self.resolve_parts(&source_parents) else {
            return false;
        };
        let Some(destination_parent) = self.resolve_parts(&destination_parents) else {
            return false;
        };
        let Some((source_block, source_slot, source_index)) =
            self.find_entry(source_parent, source_name)
        else {
            return false;
        };
        let Some(source_inode) = self.inode(source_index) else {
            return false;
        };
        if !source_inode.is_file() {
            return false;
        }
        let Some((destination_block, destination_slot, destination_index)) =
            self.find_entry(destination_parent, destination_name)
        else {
            return self.rename(source, destination);
        };
        let Some(destination_inode) = self.inode(destination_index) else {
            return false;
        };
        if !destination_inode.is_file() || destination_index == source_index {
            return false;
        }
        // Detach the source name first: if the device dies after this, the
        // old destination is still intact and the source inode is a leak.
        if self.dir_remove(source_parent, source_name).is_none() {
            return false;
        }
        if !self.write_entry(destination_block, destination_slot, source_index, destination_name) {
            let _ = self.write_entry(source_block, source_slot, source_index, source_name);
            return false;
        }
        // The destination inode is unreachable now; release it. A failure
        // here leaks blocks (the safe direction) but the replacement holds.
        if self.truncate_inode(destination_index, destination_inode) {
            let freed = Inode {
                kind: INODE_KIND_FREE,
                size: 0,
                blocks: [0; DIRECT_BLOCKS],
                indirect: 0,
                double: 0,
            };
            let _ = self.write_inode(destination_index, freed);
        }
        true
    }

    fn create(&self, path: &str, kind: u32) -> Option<u32> {
        let (parents, name) = split_path(path)?;
        let parent = self.resolve_parts(&parents)?;
        if let Some(existing) = self.find_child(parent, name) {
            let inode = self.inode(existing)?;
            if inode.kind != kind {
                return None;
            }
            if kind == INODE_KIND_FILE && !self.truncate_inode(existing, inode) {
                return None;
            }
            return Some(existing);
        }
        let index = self.allocate_inode(kind)?;
        let inode = Inode {
            kind,
            size: 0,
            blocks: [0; DIRECT_BLOCKS],
            indirect: 0,
            double: 0,
        };
        if !self.write_inode(index, inode) || !self.dir_add(parent, name, index) {
            return None;
        }
        Some(index)
    }

    fn write_file(
        &self,
        index: u32,
        offset: usize,
        bytes: &[u8],
        append: bool,
    ) -> Option<(usize, usize)> {
        let mut inode = self.inode(index)?;
        if !inode.is_file() {
            return None;
        }
        let start = if append { inode.size as usize } else { offset };
        let end = start.checked_add(bytes.len())?;
        if end > MAX_FILE_BLOCKS * BLOCK_SIZE {
            return None;
        }
        let needed = end.div_ceil(BLOCK_SIZE);
        for slot in 0..needed {
            if self.block_at(&inode, slot).is_none() {
                let block = self.allocate_block()?;
                if !zero_block(block) {
                    let _ = self.free_block(block);
                    return None;
                }
                if !self.install_block(&mut inode, slot, block) {
                    let _ = self.free_block(block);
                    return None;
                }
            }
        }
        let mut position = 0usize;
        while position < bytes.len() {
            let absolute = start + position;
            let slot = absolute / BLOCK_SIZE;
            let within = absolute % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(bytes.len() - position);
            let block = self.block_at(&inode, slot)?;
            let mut data = [0_u8; BLOCK_SIZE];
            if !read_block(block, &mut data) {
                return None;
            }
            data[within..within + take].copy_from_slice(&bytes[position..position + take]);
            if !write_block(block, &data) {
                return None;
            }
            position += take;
        }
        if end > inode.size as usize {
            inode.size = end as u32;
            if !self.write_inode(index, inode) {
                return None;
            }
        }
        Some((bytes.len(), end))
    }

    fn remove(&self, path: &str, directory: bool) -> bool {
        let Some((parents, name)) = split_path(path) else {
            return false;
        };
        let Some(parent) = self.resolve_parts(&parents) else {
            return false;
        };
        let Some(child) = self.find_child(parent, name) else {
            return false;
        };
        let Some(inode) = self.inode(child) else {
            return false;
        };
        if directory != inode.is_dir() {
            return false;
        }
        if directory {
            for slot in 0..MAX_FILE_BLOCKS {
                let Some(block) = self.block_at(&inode, slot) else {
                    break;
                };
                let mut bytes = [0_u8; BLOCK_SIZE];
                if !read_block(block, &mut bytes) {
                    return false;
                }
                for entry in 0..DIR_ENTRIES_PER_BLOCK {
                    if u32_at(&bytes, entry * DIR_ENTRY_SIZE) != 0 {
                        return false;
                    }
                }
            }
        }
        if self.dir_remove(parent, name).is_none() {
            return false;
        }
        if !self.truncate_inode(child, inode) {
            return false;
        }
        let freed = Inode {
            kind: INODE_KIND_FREE,
            size: 0,
            blocks: [0; DIRECT_BLOCKS],
            indirect: 0,
            double: 0,
        };
        self.write_inode(child, freed)
    }
}

pub fn loaded() -> bool {
    STATE.lock().superblock.is_some()
}

/// Mounts the on-disk SFS in streaming mode. Only block 0 and the root inode
/// are touched up front; everything else is read on demand.
pub fn load() -> bool {
    if !virtio_blk::init() {
        console::write_str("KSFS: no block device\r\n");
        return false;
    }
    let device_blocks = virtio_blk::sector_count();
    if device_blocks == 0 || device_blocks > u32::MAX as u64 {
        console::write_str("KSFS: bad device size\r\n");
        return false;
    }
    let mut superblock_bytes = [0_u8; BLOCK_SIZE];
    if !virtio_blk::read_sector(0, &mut superblock_bytes) {
        console::write_str("KSFS: superblock read failed\r\n");
        return false;
    }
    let Some(superblock) =
        parse_superblock(&superblock_bytes, device_blocks as u32)
    else {
        console::write_str("KSFS: bad superblock\r\n");
        return false;
    };
    {
        let mut state = STATE.lock();
        state.superblock = Some(superblock);
        state.device_blocks = device_blocks as u32;
    }
    let Some(root) = reader().and_then(|reader| reader.inode(superblock.root_inode)) else {
        console::write_str("KSFS: root inode unreadable\r\n");
        STATE.lock().superblock = None;
        return false;
    };
    if !root.is_dir() {
        console::write_str("KSFS: root is not a directory\r\n");
        STATE.lock().superblock = None;
        return false;
    }
    console::write_str("KSFS: streaming, blocks=");
    console::write_dec_u64(u64::from(superblock.total_blocks));
    console::write_str(" inodes=");
    console::write_dec_u64(u64::from(superblock.inode_count));
    console::write_str(" cache=");
    console::write_dec_u64(CACHE_BLOCKS as u64);
    console::write_str(" blocks\r\n");
    true
}

/// Diagnostic summary printed once at boot.
pub fn report() -> bool {
    let Some(reader) = reader() else {
        console::write_str("KSFS: not mounted\r\n");
        return false;
    };
    console::write_str("KSFS: mounted, blocks=");
    console::write_dec_u64(u64::from(reader.superblock.total_blocks));
    console::write_str("\r\n");
    true
}

pub struct Sfs;

impl FileSystem for Sfs {
    fn create_file(&mut self, path: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        reader.create(path, INODE_KIND_FILE).map(|_| ()).ok_or(())
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        reader.create(path, INODE_KIND_DIR).map(|_| ()).ok_or(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        if reader.remove(path, false) { Ok(()) } else { Err(()) }
    }

    fn remove_directory(&mut self, path: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        if reader.remove(path, true) { Ok(()) } else { Err(()) }
    }

    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        if reader.rename(source, destination) { Ok(()) } else { Err(()) }
    }

    fn replace(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        if reader.replace(source, destination) { Ok(()) } else { Err(()) }
    }

    fn is_directory(&self, path: &str) -> Result<bool, ()> {
        let reader = reader().ok_or(())?;
        Ok(reader.is_directory(path))
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()> {
        let reader = reader().ok_or(())?;
        let index = reader.resolve(path).ok_or(())?;
        reader.entries(index).ok_or(())
    }

    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Result<Vec<u8>, ()> {
        let reader = reader().ok_or(())?;
        reader.read_at(path, offset, limit).ok_or(())
    }

    fn write_at(
        &mut self,
        path: &str,
        offset: usize,
        bytes: &[u8],
        append: bool,
    ) -> Result<(usize, usize), ()> {
        let reader = reader().ok_or(())?;
        let index = reader.resolve(path).ok_or(())?;
        reader
            .write_file(index, offset, bytes, append)
            .ok_or(())
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let reader = reader().ok_or(())?;
        reader.len(path).ok_or(())
    }

    fn seed_file(&mut self, path: &str, contents: &[u8]) -> Result<(), ()> {
        let reader = reader().ok_or(())?;
        let index = reader.create(path, INODE_KIND_FILE).ok_or(())?;
        reader
            .write_file(index, 0, contents, false)
            .map(|_| ())
            .ok_or(())
    }

    fn usage(&self, _path: &str) -> Option<FsUsage> {
        reader()?.usage()
    }

    fn usage_supported(&self) -> bool {
        true
    }
}
