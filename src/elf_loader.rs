//! Minimal ELF64 loader for static AArch64 userspace executables.
//!
//! The loader accepts only bounded PT_LOAD segments inside the 2 MB user
//! image window. Every task receives fresh physical pages; segment contents
//! and zero-filled BSS are copied through the kernel's high-half alias.

use alloc::{vec, vec::Vec};

use crate::{
    frame_allocator,
    memory::{phys_to_virt, PAGE_SIZE},
    pl011,
};

pub const USER_IMAGE_VA: usize = 0x4000_0000;
const USER_IMAGE_SIZE: usize = 0x20_0000;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PROGRAM_HEADER_SIZE: usize = 56;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PageKind {
    Text,
    ReadOnly,
    Data,
}

pub struct UserImage {
    pub entry: usize,
    pub physical_start: usize,
    pub page_count: usize,
    pub kinds: Vec<PageKind>,
}

impl UserImage {
    pub fn release(&self) {
        for index in 0..self.page_count {
            frame_allocator::free_page(self.physical_start + index * PAGE_SIZE);
        }
    }
}

#[derive(Copy, Clone)]
struct Segment {
    file_offset: usize,
    virtual_address: usize,
    file_size: usize,
    memory_size: usize,
    kind: PageKind,
}

pub fn load(bytes: &[u8]) -> Option<UserImage> {
    if bytes.get(0..4)? != b"\x7FELF" || bytes.get(4) != Some(&2) || bytes.get(5) != Some(&1) {
        return None;
    }
    if read_u16(bytes, 16)? != ET_EXEC || read_u16(bytes, 18)? != EM_AARCH64 {
        return None;
    }

    // Temporary bring-up probes: the caller prints "load elf" before this call,
    // so every step below is reported to pin down where a spawn stalls. Remove
    // once sfs_server mounts reliably.
    pl011::write_str("ELF: begin size=");
    pl011::write_dec_u64(bytes.len() as u64);
    pl011::write_str("\r\n");

    let entry = usize::try_from(read_u64(bytes, 24)?).ok()?;
    let program_offset = usize::try_from(read_u64(bytes, 32)?).ok()?;
    let program_size = read_u16(bytes, 54)? as usize;
    let program_count = read_u16(bytes, 56)? as usize;
    if program_size < PROGRAM_HEADER_SIZE || program_count == 0 {
        pl011::write_str("ELF: reject - bad program header size/count\r\n");
        return None;
    }
    pl011::write_str("ELF: header ok entry=");
    pl011::write_hex_u64(entry as u64);
    pl011::write_str(" phnum=");
    pl011::write_dec_u64(program_count as u64);
    pl011::write_str("\r\n");

    let mut segments = Vec::new();
    let mut image_end = USER_IMAGE_VA;
    for index in 0..program_count {
        let header_offset = program_offset.checked_add(index.checked_mul(program_size)?)?;
        let header = bytes.get(header_offset..header_offset.checked_add(PROGRAM_HEADER_SIZE)?)?;
        if read_u32(header, 0)? != PT_LOAD {
            continue;
        }

        let flags = read_u32(header, 4)?;
        let file_offset = usize::try_from(read_u64(header, 8)?).ok()?;
        let virtual_address = usize::try_from(read_u64(header, 16)?).ok()?;
        let file_size = usize::try_from(read_u64(header, 32)?).ok()?;
        let memory_size = usize::try_from(read_u64(header, 40)?).ok()?;

        pl011::write_str("ELF SEGMENT: vaddr=");
        pl011::write_hex_u64(virtual_address as u64);
        pl011::write_str(" filesz=");
        pl011::write_hex_u64(file_size as u64);
        pl011::write_str(" memsz=");
        pl011::write_hex_u64(memory_size as u64);
        pl011::write_str(" flags=");
        pl011::write_dec_u64(flags as u64);
        pl011::write_str("\r\n");

        if flags & PF_R == 0 || flags & PF_W != 0 && flags & PF_X != 0 {
            pl011::write_str("ELF: reject - segment flags unusable (W^X)\r\n");
            return None;
        }
        let kind = if flags & PF_X != 0 {
            PageKind::Text
        } else if flags & PF_W != 0 {
            PageKind::Data
        } else {
            PageKind::ReadOnly
        };
        if memory_size == 0 {
            continue;
        }
        if file_size > memory_size
            || virtual_address < USER_IMAGE_VA
            || virtual_address.checked_add(memory_size)? > USER_IMAGE_VA + USER_IMAGE_SIZE
            || file_offset.checked_add(file_size)? > bytes.len()
        {
            pl011::write_str("ELF: reject - segment outside the user window\r\n");
            return None;
        }
        image_end = image_end.max(virtual_address.checked_add(memory_size)?);
        segments.push(Segment {
            file_offset,
            virtual_address,
            file_size,
            memory_size,
            kind,
        });
    }

    if segments.is_empty()
        || !segments.iter().any(|segment| {
            segment.kind == PageKind::Text
                && entry >= segment.virtual_address
                && entry < segment.virtual_address + segment.memory_size
        })
    {
        pl011::write_str("ELF: reject - entry outside every text segment\r\n");
        return None;
    }

    let image_size = image_end.checked_sub(USER_IMAGE_VA)?;
    let page_count = image_size.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
    if page_count == 0 || page_count > USER_IMAGE_SIZE / PAGE_SIZE {
        pl011::write_str("ELF: reject - image size out of range\r\n");
        return None;
    }
    pl011::write_str("ELF: image_size=");
    pl011::write_hex_u64(image_size as u64);
    pl011::write_str(" pages=");
    pl011::write_dec_u64(page_count as u64);
    pl011::write_str("\r\n");
    let mut kinds = vec![PageKind::ReadOnly; page_count];
    let mut mapped = vec![false; page_count];
    for segment in &segments {
        let first = (segment.virtual_address - USER_IMAGE_VA) / PAGE_SIZE;
        let last = (segment.virtual_address - USER_IMAGE_VA + segment.memory_size - 1) / PAGE_SIZE;
        for page in first..=last {
            if mapped[page] && kinds[page] != segment.kind {
                return None;
            }
            mapped[page] = true;
            kinds[page] = segment.kind;
        }
    }
    if mapped.iter().any(|mapped| !mapped) {
        pl011::write_str("ELF: reject - image has a hole no segment covers\r\n");
        return None;
    }

    pl011::write_str("ELF: alloc run pages=");
    pl011::write_dec_u64(page_count as u64);
    pl011::write_str("\r\n");
    let Some(physical_start) = frame_allocator::alloc_contiguous_pages(page_count) else {
        pl011::write_str("ELF: reject - no contiguous physical run\r\n");
        return None;
    };
    pl011::write_str("ELF: run at ");
    pl011::write_hex_u64(physical_start as u64);
    pl011::write_str("\r\n");
    unsafe {
        core::ptr::write_bytes(
            phys_to_virt(physical_start) as *mut u8,
            0,
            page_count * PAGE_SIZE,
        );
        for segment in &segments {
            let source = bytes.as_ptr().add(segment.file_offset);
            let destination =
                phys_to_virt(physical_start + segment.virtual_address - USER_IMAGE_VA) as *mut u8;
            core::ptr::copy_nonoverlapping(source, destination, segment.file_size);
        }
    }
    pl011::write_str("ELF: zero+copy done\r\n");

    pl011::write_str("ELF: done\r\n");
    Some(UserImage {
        entry,
        physical_start,
        page_count,
        kinds,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}
