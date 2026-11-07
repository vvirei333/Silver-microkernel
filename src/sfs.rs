//! Kernel SFS: device glue and the VFS `FileSystem` implementation.
//!
//! All on-disk format rules live in [`crate::sfs_format`], parameterized by
//! the [`sfs_format::SfsDisk`] trait so they are testable on the host; this
//! module owns the image buffer, the virtio-backed disk adapter, and the
//! write-through discipline: the in-memory image stays authoritative for
//! reads, every mutation is written through to the device sector by sector.
//!
//! The kernel owns the disk (`virtio_blk`) and loads the whole image at boot.
//! Write syscalls are gated by `CAP_FS_WRITE` upstream (see `syscall.rs`).

use alloc::vec::Vec;

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;
use crate::sfs_format::{self, SfsDisk};
use crate::vfs::{DirEntry, FileSystem, MAX_NAME_LEN};
use crate::virtio_blk;

use sfs_format::{Superblock, BLOCK_SIZE};

/// The format's fixed geometry; used to size the boot read before the
/// superblock is parsed.
const IMAGE_SECTORS: usize = 2048;

static mut IMAGE_PTR: *mut u8 = core::ptr::null_mut();
static mut IMAGE_LEN: usize = 0;

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

/// Persists format mutations through the kernel's virtio-blk driver.
struct Disk;

impl SfsDisk for Disk {
    fn write_sector(&mut self, sector: u64, data: &[u8; BLOCK_SIZE]) -> bool {
        virtio_blk::write_sector(sector, data)
    }
}

pub struct Sfs;

impl FileSystem for Sfs {
    fn create_file(&mut self, path: &str) -> Result<(), ()> {
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        match sfs_format::find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);
                let (kind, _, _, _, _) = sfs_format::inode_get(image, &superblock, index).ok_or(())?;
                if kind != sfs_format::INODE_KIND_FILE {
                    return Err(());
                }
                if sfs_format::truncate(image, &superblock, &mut Disk, index) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            None => sfs_format::create_inode(image, &superblock, &mut Disk, parent, name, sfs_format::INODE_KIND_FILE)
                .map(|_| ())
                .ok_or(()),
        }
    }

    fn make_directory(&mut self, path: &str) -> Result<(), ()> {
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        if sfs_format::find_entry(image, &superblock, parent, name).is_some() {
            return Err(());
        }
        sfs_format::create_inode(image, &superblock, &mut Disk, parent, name, sfs_format::INODE_KIND_DIR)
            .map(|_| ())
            .ok_or(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), ()> {
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let (block, slot) = sfs_format::find_entry(image, &superblock, parent, name).ok_or(())?;
        let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);
        let (kind, size, blocks, indirect, double) =
            sfs_format::inode_get(image, &superblock, index).ok_or(())?;
        if kind != sfs_format::INODE_KIND_FILE {
            return Err(());
        }
        if !sfs_format::remove_dir_entry(image, &superblock, &mut Disk, parent, block, slot) {
            return Err(());
        }
        let empty = [0_u32; sfs_format::DIRECT_BLOCKS];
        sfs_format::inode_put(image, &superblock, &mut Disk, index, sfs_format::INODE_KIND_FREE, 0, &empty, 0, 0);
        // Release only after the inode stopped referencing the blocks: a crash
        // in between leaks them instead of cross-linking two files.
        sfs_format::free_inode_blocks(image, &superblock, kind, size, &blocks, indirect, double);
        sfs_format::flush_bitmap(image, &superblock, &mut Disk);
        Ok(())
    }

    fn remove_directory(&mut self, path: &str) -> Result<(), ()> {
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let (block, slot) = sfs_format::find_entry(image, &superblock, parent, name).ok_or(())?;
        let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);
        let (kind, _, blocks, indirect, _) =
            sfs_format::inode_get(image, &superblock, index).ok_or(())?;
        // A directory that ever held an entry keeps its block, because entries
        // are cleared in place; only a never-used directory is removable.
        if kind != sfs_format::INODE_KIND_DIR
            || indirect != 0
            || blocks.iter().any(|&block| block != 0)
        {
            return Err(());
        }
        if !sfs_format::remove_dir_entry(image, &superblock, &mut Disk, parent, block, slot) {
            return Err(());
        }
        let empty = [0_u32; sfs_format::DIRECT_BLOCKS];
        sfs_format::inode_put(image, &superblock, &mut Disk, index, sfs_format::INODE_KIND_FREE, 0, &empty, 0, 0);
        sfs_format::flush_bitmap(image, &superblock, &mut Disk);
        Ok(())
    }

    fn rename(&mut self, source: &str, destination: &str) -> Result<(), ()> {
        let source_parts = sfs_format::split_path(source)?;
        let dest_parts = sfs_format::split_path(destination)?;
        let (&old_name, old_parent_parts) = source_parts.split_last().ok_or(())?;
        let (&new_name, new_parent_parts) = dest_parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let old_parent = sfs_format::resolve_dir(image, &superblock, old_parent_parts).ok_or(())?;
        let new_parent = sfs_format::resolve_dir(image, &superblock, new_parent_parts).ok_or(())?;
        if sfs_format::find_entry(image, &superblock, new_parent, new_name).is_some() {
            return Err(());
        }
        let (block, slot) = sfs_format::find_entry(image, &superblock, old_parent, old_name).ok_or(())?;
        let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);

        if old_parent == new_parent {
            if !sfs_format::rename_entry_in_place(image, &superblock, &mut Disk, block, slot, new_name) {
                return Err(());
            }
            return Ok(());
        }

        sfs_format::add_entry(image, &superblock, &mut Disk, new_parent, new_name, index).ok_or(())?;
        if !sfs_format::remove_dir_entry(image, &superblock, &mut Disk, old_parent, block, slot) {
            // The entry now exists under both parents. Undo the new one so a
            // failed rename never leaves a hard link behind, and report the
            // operation as failed.
            let _ = undo_new_entry(image, &superblock, new_parent, new_name);
            return Err(());
        }
        Ok(())
    }

    fn is_directory(&self, path: &str) -> Result<bool, ()> {
        let image = image()?;
        let superblock = sfs_format::superblock(image)?;
        let parts = sfs_format::split_path(path)?;
        if parts.is_empty() {
            return Ok(true);
        }
        let (_, kind, _) = sfs_format::lookup(image, &superblock, path).ok_or(())?;
        Ok(kind == sfs_format::INODE_KIND_DIR)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, ()> {
        let image = image()?;
        let superblock = sfs_format::superblock(image)?;
        let parts = sfs_format::split_path(path)?;
        let dir = sfs_format::resolve_dir(image, &superblock, &parts).ok_or(())?;
        let (_, _, blocks, indirect, _) = sfs_format::inode_get(image, &superblock, dir).ok_or(())?;
        let mut entries = Vec::new();
        for block_slot in 0..sfs_format::MAX_FILE_BLOCKS {
            let Some(block) =
                sfs_format::inode_slot_block(image, &superblock, &blocks, indirect, 0, block_slot)
            else {
                break;
            };
            let start = sfs_format::block_start(image, &superblock, block).ok_or(())?;
            for slot in 0..sfs_format::DIR_ENTRIES_PER_BLOCK {
                let entry = start + slot * sfs_format::DIR_ENTRY_SIZE;
                let index = sfs_format::u32_at(image, entry);
                if index == 0 {
                    continue;
                }
                let length = image[entry + 4] as usize;
                if length == 0 || length > sfs_format::NAME_MAX {
                    continue;
                }
                let (kind, _, _, _, _) = sfs_format::inode_get(image, &superblock, index).ok_or(())?;
                let mut name = [0_u8; MAX_NAME_LEN];
                name[..length].copy_from_slice(&image[entry + 5..entry + 5 + length]);
                entries.push(DirEntry {
                    name,
                    name_len: length as u64,
                    kind: if kind == sfs_format::INODE_KIND_DIR { 2 } else { 1 },
                });
            }
        }
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: usize, limit: usize) -> Result<Vec<u8>, ()> {
        let image = image()?;
        let superblock = sfs_format::superblock(image)?;
        let (index, _, size) = sfs_format::lookup(image, &superblock, path).ok_or(())?;
        if offset >= size as usize {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(limit).min(size as usize);
        let (_, _, blocks, indirect, double) = sfs_format::inode_get(image, &superblock, index).ok_or(())?;
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let slot = position / BLOCK_SIZE;
            let block = sfs_format::inode_slot_block(image, &superblock, &blocks, indirect, double, slot)
                .ok_or(())?;
            let start = sfs_format::block_start(image, &superblock, block).ok_or(())?;
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
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let index = match sfs_format::find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);
                let (kind, _, _, _, _) = sfs_format::inode_get(image, &superblock, index).ok_or(())?;
                if kind != sfs_format::INODE_KIND_FILE {
                    return Err(());
                }
                index
            }
            None => sfs_format::create_inode(image, &superblock, &mut Disk, parent, name, sfs_format::INODE_KIND_FILE)
                .ok_or(())?,
        };

        let (_, current_size, old_blocks, old_indirect, old_double) =
            sfs_format::inode_get(image, &superblock, index).ok_or(())?;
        let base = if append {
            current_size as usize
        } else if offset == 0 {
            if !sfs_format::truncate(image, &superblock, &mut Disk, index) {
                return Err(());
            }
            0
        } else {
            if offset > current_size as usize {
                return Err(());
            }
            offset
        };

        // Fast path: a positioned write that stays inside the existing block
        // map touches only the covered data sectors — no reallocation, no
        // inode rewrite, no bitmap churn.
        if !append && offset != 0 && base + bytes.len() <= current_size as usize {
            if sfs_format::overwrite_in_place(image, &superblock, &mut Disk, index, offset, bytes) {
                return Ok((bytes.len(), base + bytes.len()));
            }
            // Fall through to the full rewrite on any fast-path refusal.
        }

        if append || offset != 0 {
            let mut content = sfs_format::read_inode_bytes(image, &superblock, index).ok_or(())?;
            let end = base.saturating_add(bytes.len());
            if end > content.len() {
                content.resize(end, 0);
            }
            content[base..end].copy_from_slice(bytes);
            if !sfs_format::store(
                image,
                &superblock,
                &mut Disk,
                index,
                &content,
                &old_blocks,
                old_indirect,
                old_double,
                current_size,
            ) {
                return Err(());
            }
            return Ok((bytes.len(), end));
        }

        let empty = [0_u32; sfs_format::DIRECT_BLOCKS];
        if !sfs_format::store(image, &superblock, &mut Disk, index, bytes, &empty, 0, 0, 0) {
            return Err(());
        }
        Ok((bytes.len(), bytes.len()))
    }

    fn len(&self, path: &str) -> Result<usize, ()> {
        let image = image()?;
        let superblock = sfs_format::superblock(image)?;
        if sfs_format::split_path(path)?.is_empty() {
            return Ok(0);
        }
        let (_, kind, size) = sfs_format::lookup(image, &superblock, path).ok_or(())?;
        if kind != sfs_format::INODE_KIND_FILE {
            return Err(());
        }
        Ok(size as usize)
    }

    fn seed_file(&mut self, path: &str, contents: &[u8]) -> Result<(), ()> {
        if contents.len() > sfs_format::MAX_FILE_BLOCKS * BLOCK_SIZE {
            return Err(());
        }
        let parts = sfs_format::split_path(path)?;
        let (&name, parent_parts) = parts.split_last().ok_or(())?;
        let image = image_mut()?;
        let superblock = sfs_format::superblock(image)?;
        let parent = sfs_format::resolve_dir(image, &superblock, parent_parts).ok_or(())?;
        let index = match sfs_format::find_entry(image, &superblock, parent, name) {
            Some((block, slot)) => {
                let index = sfs_format::u32_at(image, block * BLOCK_SIZE + slot * sfs_format::DIR_ENTRY_SIZE);
                let (kind, _, _, _, _) = sfs_format::inode_get(image, &superblock, index).ok_or(())?;
                if kind != sfs_format::INODE_KIND_FILE {
                    return Err(());
                }
                index
            }
            None => sfs_format::create_inode(image, &superblock, &mut Disk, parent, name, sfs_format::INODE_KIND_FILE)
                .ok_or(())?,
        };
        if !sfs_format::truncate(image, &superblock, &mut Disk, index) {
            return Err(());
        }
        let empty = [0_u32; sfs_format::DIRECT_BLOCKS];
        if !sfs_format::store(image, &superblock, &mut Disk, index, contents, &empty, 0, 0, 0) {
            return Err(());
        }
        sfs_format::flush_bitmap(image, &superblock, &mut Disk);
        Ok(())
    }

    fn usage(&self, path: &str) -> Option<crate::vfs::FsUsage> {
        let image = image().ok()?;
        let superblock = sfs_format::superblock(image).ok()?;
        let usage = sfs_format::usage_under(image, &superblock, path)?;
        Some(crate::vfs::FsUsage {
            inodes: usage.inodes,
            data_blocks: usage.data_blocks,
            metadata_blocks: usage.metadata_blocks,
        })
    }
}

/// Undo of a cross-parent rename's first half: removes the entry `add_entry`
/// just created. Best effort — the caller already reports failure.
fn undo_new_entry(
    image: &mut [u8],
    superblock: &Superblock,
    parent: u32,
    name: &str,
) -> Result<(), ()> {
    let (block, slot) = sfs_format::find_entry(image, superblock, parent, name).ok_or(())?;
    if !sfs_format::remove_dir_entry(image, superblock, &mut Disk, parent, block, slot) {
        return Err(());
    }
    Ok(())
}

/// Upper bound on the loaded image: the format keeps the whole image in
/// kernel RAM, so the cap is the frame budget, not the disk. 8 MiB covers
/// the app bundles with wide headroom; bigger disks need sparse reads first.
const MAX_IMAGE_SECTORS: usize = 16384;

/// Reads the whole image into kernel memory and keeps ownership of the disk.
/// The read size adapts to the device capacity (clamped to
/// [`MAX_IMAGE_SECTORS`]), so both the legacy 1 MiB disk and larger ones
/// mount; the superblock decides what is really used.
pub fn load() -> bool {
    if !virtio_blk::init() {
        console::write_str("KSFS: no block device\r\n");
        return false;
    }
    let capacity = virtio_blk::sector_count() as usize;
    let device_sectors = if capacity == 0 {
        IMAGE_SECTORS
    } else {
        capacity
    };
    let mut sectors = device_sectors.min(MAX_IMAGE_SECTORS);
    let mut physical_tag: Option<(usize, u32)> = None;
    while sectors >= IMAGE_SECTORS {
        if let Some(allocation) = frame_allocator::alloc_contiguous_pages_tagged(sectors * BLOCK_SIZE / 4096) {
            physical_tag = Some(allocation);
            break;
        }
        // Not enough contiguous frames for this size: halve and retry, down
        // to the legacy geometry.
        sectors /= 2;
    }
    let Some((physical, _tag)) = physical_tag else {
        console::write_str("KSFS: image allocation failed\r\n");
        return false;
    };
    console::write_str("KSFS: image at 0x");
    console::write_hex_u64(physical as u64);
    console::write_str(" sectors=");
    console::write_dec_u64(sectors as u64);
    console::write_str("\r\n");
    let bytes = sectors * BLOCK_SIZE;
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
    let check = sfs_format::superblock(snapshot).and_then(|superblock| {
        sfs_format::fsck(snapshot, &superblock).map(|report| (superblock, report))
    });
    let (superblock, report) = match check {
        Ok(found) => found,
        Err(()) => {
            unsafe {
                IMAGE_PTR = core::ptr::null_mut();
                IMAGE_LEN = 0;
            }
            console::write_str("KSFS: image inconsistent\r\n");
            return false;
        }
    };
    if report.leaks > 0 {
        console::write_str("KSFS: ");
        console::write_dec_u64(report.leaks as u64);
        console::write_str(" leaked block(s)\r\n");
    }
    let _ = superblock;
    true
}

/// Diagnostic summary printed once at boot.
pub fn report() -> bool {
    let Ok(image) = image() else {
        return false;
    };
    let Ok(superblock) = sfs_format::superblock(image) else {
        console::write_str("KSFS: bad superblock\r\n");
        return false;
    };
    console::write_str("KSFS: mounted, blocks=");
    console::write_dec_u64(u64::from(superblock.total_blocks));
    console::write_str("\r\n");
    true
}
