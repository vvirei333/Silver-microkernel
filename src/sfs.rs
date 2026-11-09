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

    fn replace(&mut self, _source: &str, _destination: &str) -> Result<(), ()> {
        Err(())
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
        _path: &str,
        _offset: usize,
        _bytes: &[u8],
        _append: bool,
    ) -> Result<(usize, usize), ()> {
        Err(())
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let reader = reader().ok_or(())?;
        reader.len(path).ok_or(())
    }

    fn seed_file(&mut self, _path: &str, _contents: &[u8]) -> Result<(), ()> {
        Err(())
    }

    fn usage(&self, _path: &str) -> Option<FsUsage> {
        reader()?.usage()
    }

    fn usage_supported(&self) -> bool {
        true
    }
}
