//! ELF64 loader for dynamically loaded (SFS-backed) Ring 3 programs.
//!
//! This is the *new* loading path for Phase 2. It parses a static `ET_EXEC`
//! AArch64 ELF64 image, maps its `PT_LOAD` segments into fresh physical pages,
//! copies file data and zero-fills BSS, and hands back a [`LoadedImage`] that
//! the scheduler installs into a task's TTBR0.
//!
//! It is intentionally self-contained and does **not** touch the existing
//! embedded path (`elf_loader.rs` / `user_program::image`): those keep serving
//! the programs compiled into the kernel until the dynamic path is complete.
//!
//! Layout contract (must match `userspace/linker.ld`):
//!
//! * base virtual address `0x4000_0000`, three `PT_LOAD` segments —
//!   `text` (R-X), `rodata` (R--) and `data` (RW-, with `NOLOAD` `.bss`);
//! * every byte of the 2 MiB image window is covered (no holes), so a single
//!   contiguous physical run is enough and matches how `create_task_space`
//!   installs the image L3 table.
//!
//! Error model: unlike `elf_loader.rs`, which returns `Option` and logs every
//! step to the UART, this loader returns a structured [`ElfError`]. All
//! validation happens *before* any frame is allocated, so an error can never
//! leak physical pages.

#![allow(dead_code)] // Scaffold: `load_elf` is wired into the scheduler in the next step.

use alloc::vec;
use alloc::vec::Vec;

use crate::{
    frame_allocator,
    memory::{phys_to_virt, PAGE_SIZE},
};

/// Base of the 2 MiB user image window. Linked to by `userspace/linker.ld`
/// (`. = 0x40000000`) and mirrored by `elf_loader::USER_IMAGE_VA`.
pub const USER_IMAGE_VA: usize = 0x4000_0000;
/// Size of the image window; `elf_loader` uses the same bound.
pub const USER_IMAGE_SIZE: usize = 0x20_0000;

// ELF64 identification bytes.
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PHDR_SIZE: usize = 56;

/// Why a candidate image was rejected. Kept `Copy` so callers can report the
/// exact stage that failed without allocating.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ElfError {
    BadMagic,
    BadClass,
    BadDataEncoding,
    NotExecutable,
    WrongMachine,
    Truncated,
    NoProgramHeaders,
    BadProgramHeader,
    /// A segment is both writable and executable (W^X violation).
    WritableExecutable,
    /// A segment has neither R nor a usable R/W/RX combination.
    NoReadPermission,
    /// `p_filesz > p_memsz`.
    FileLargerThanMemory,
    SegmentOutOfRange,
    NoLoadSegments,
    EntryOutsideText,
    ImageTooLarge,
    /// Two segments cover the same page with conflicting permissions.
    OverlappingKinds,
    /// A page inside the image window is covered by no segment.
    HoleInImage,
    OutOfMemory,
}

/// Page permission classes produced by the linker script's three segments.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PageKind {
    /// `R-X`: code, executable and readable, never writable.
    Text,
    /// `R--`: constants, readable only.
    ReadOnly,
    /// `RW-`: mutable data + zero-filled BSS, never executable.
    Data,
}

/// ELF64 file header (`Elf64_Ehdr`). `#[repr(C)]` yields the exact on-disk
/// layout because every field is naturally aligned, but the fields are filled
/// with checked byte reads — the source buffer from SFS is not guaranteed to be
/// aligned, so it is never reinterpreted through a pointer cast.
#[repr(C)]
pub struct Elf64Ehdr {
    pub ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

impl Elf64Ehdr {
    fn parse(bytes: &[u8]) -> Result<Self, ElfError> {
        let mut ident = [0_u8; 16];
        ident.copy_from_slice(bytes.get(..16).ok_or(ElfError::Truncated)?);
        Ok(Self {
            ident,
            e_type: read_u16(bytes, 16)?,
            e_machine: read_u16(bytes, 18)?,
            e_version: read_u32(bytes, 20)?,
            e_entry: read_u64(bytes, 24)?,
            e_phoff: read_u64(bytes, 32)?,
            e_shoff: read_u64(bytes, 40)?,
            e_flags: read_u32(bytes, 48)?,
            e_ehsize: read_u16(bytes, 52)?,
            e_phentsize: read_u16(bytes, 54)?,
            e_phnum: read_u16(bytes, 56)?,
            e_shentsize: read_u16(bytes, 58)?,
            e_shnum: read_u16(bytes, 60)?,
            e_shstrndx: read_u16(bytes, 62)?,
        })
    }
}

/// ELF64 program header (`Elf64_Phdr`). See [`Elf64Ehdr`] for why `#[repr(C)]`
/// is safe and why fields are filled with checked byte reads.
#[repr(C)]
pub struct Elf64Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl Elf64Phdr {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self, ElfError> {
        let end = offset.checked_add(PHDR_SIZE).ok_or(ElfError::Truncated)?;
        let ph = bytes.get(offset..end).ok_or(ElfError::Truncated)?;
        Ok(Self {
            p_type: read_u32(ph, 0)?,
            p_flags: read_u32(ph, 4)?,
            p_offset: read_u64(ph, 8)?,
            p_vaddr: read_u64(ph, 16)?,
            p_paddr: read_u64(ph, 24)?,
            p_filesz: read_u64(ph, 32)?,
            p_memsz: read_u64(ph, 40)?,
            p_align: read_u64(ph, 48)?,
        })
    }
}

/// One loaded `PT_LOAD` segment, reduced to the fields the loader needs.
struct Segment {
    file_offset: usize,
    virtual_address: usize,
    file_size: usize,
    memory_size: usize,
    kind: PageKind,
}

/// A fully loaded image: fresh contiguous physical pages holding the copied
/// segment bytes and zeroed BSS, plus the metadata the scheduler needs to
/// install the page table and to release the frames on task teardown.
pub struct LoadedImage {
    /// Program entry point (a virtual address in the image window).
    pub entry: usize,
    /// Top of the used image window: the highest `p_vaddr + p_memsz`.
    pub upper_bound: usize,
    /// Physical address of the first image page.
    pub physical_start: usize,
    /// Number of pages in the image run.
    pub page_count: usize,
    /// Per-page permission class, index `0` is `USER_IMAGE_VA`.
    pub kinds: Vec<PageKind>,
}

impl LoadedImage {
    /// Returns the physical frames to the allocator. Called by the scheduler
    /// on task teardown or when a later `create_task_space` step fails.
    pub fn release(&self) {
        for index in 0..self.page_count {
            frame_allocator::free_page(self.physical_start + index * PAGE_SIZE);
        }
    }
}

/// Parses `elf_data`, validates it, allocates and fills the image, and returns
/// the loaded image. No frames are allocated until the whole image has been
/// validated, so `Err` never leaks memory.
pub fn load_elf(elf_data: &[u8]) -> Result<LoadedImage, ElfError> {
    let header = Elf64Ehdr::parse(elf_data)?;
    if &header.ident[..4] != b"\x7FELF" {
        return Err(ElfError::BadMagic);
    }
    if header.ident[EI_CLASS] != ELFCLASS64 {
        return Err(ElfError::BadClass);
    }
    if header.ident[EI_DATA] != ELFDATA2LSB {
        return Err(ElfError::BadDataEncoding);
    }
    if header.e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if header.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongMachine);
    }

    let entry = header.e_entry as usize;
    let program_offset = header.e_phoff as usize;
    let program_size = header.e_phentsize as usize;
    let program_count = header.e_phnum as usize;
    if program_size < PHDR_SIZE || program_count == 0 {
        return Err(ElfError::NoProgramHeaders);
    }

    let mut segments = Vec::new();
    let mut image_end = USER_IMAGE_VA;
    for index in 0..program_count {
        let header_offset = program_offset
            .checked_add(index.checked_mul(program_size).ok_or(ElfError::Truncated)?)
            .ok_or(ElfError::Truncated)?;
        let ph = Elf64Phdr::parse(elf_data, header_offset)?;
        if ph.p_type != PT_LOAD {
            continue;
        }

        let flags = ph.p_flags;
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(ElfError::WritableExecutable);
        }
        if flags & PF_R == 0 {
            return Err(ElfError::NoReadPermission);
        }
        let kind = if flags & PF_X != 0 {
            PageKind::Text
        } else if flags & PF_W != 0 {
            PageKind::Data
        } else {
            PageKind::ReadOnly
        };

        let file_offset = ph.p_offset as usize;
        let virtual_address = ph.p_vaddr as usize;
        let file_size = ph.p_filesz as usize;
        let memory_size = ph.p_memsz as usize;
        if memory_size == 0 {
            continue;
        }
        if file_size > memory_size {
            return Err(ElfError::FileLargerThanMemory);
        }
        let memory_end = virtual_address
            .checked_add(memory_size)
            .ok_or(ElfError::SegmentOutOfRange)?;
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(ElfError::SegmentOutOfRange)?;
        if virtual_address < USER_IMAGE_VA
            || memory_end > USER_IMAGE_VA + USER_IMAGE_SIZE
            || file_end > elf_data.len()
        {
            return Err(ElfError::SegmentOutOfRange);
        }

        image_end = image_end.max(memory_end);
        segments.push(Segment {
            file_offset,
            virtual_address,
            file_size,
            memory_size,
            kind,
        });
    }

    if segments.is_empty() {
        return Err(ElfError::NoLoadSegments);
    }
    if !segments.iter().any(|segment| {
        segment.kind == PageKind::Text
            && entry >= segment.virtual_address
            && entry < segment.virtual_address + segment.memory_size
    }) {
        return Err(ElfError::EntryOutsideText);
    }

    let image_size = image_end
        .checked_sub(USER_IMAGE_VA)
        .ok_or(ElfError::SegmentOutOfRange)?;
    let page_count = image_size.div_ceil(PAGE_SIZE);
    if page_count == 0 || page_count > USER_IMAGE_SIZE / PAGE_SIZE {
        return Err(ElfError::ImageTooLarge);
    }

    // Assign a permission class to every page and reject holes or conflicting
    // overlaps before touching the frame allocator.
    let mut kinds = vec![PageKind::ReadOnly; page_count];
    let mut covered = vec![false; page_count];
    for segment in &segments {
        let first = (segment.virtual_address - USER_IMAGE_VA) / PAGE_SIZE;
        let last = (segment.virtual_address - USER_IMAGE_VA + segment.memory_size - 1) / PAGE_SIZE;
        for page in first..=last {
            if covered[page] && kinds[page] != segment.kind {
                return Err(ElfError::OverlappingKinds);
            }
            covered[page] = true;
            kinds[page] = segment.kind;
        }
    }
    if covered.iter().any(|covered| !covered) {
        return Err(ElfError::HoleInImage);
    }

    let physical_start = frame_allocator::alloc_contiguous_pages(page_count)
        .ok_or(ElfError::OutOfMemory)?;

    // Zero the whole run (BSS is the tail of `data` that `p_filesz` does not
    // cover), then copy each segment's file bytes over it.
    unsafe {
        core::ptr::write_bytes(
            phys_to_virt(physical_start) as *mut u8,
            0,
            page_count * PAGE_SIZE,
        );
        for segment in &segments {
            let source = elf_data.as_ptr().add(segment.file_offset);
            let destination = phys_to_virt(
                physical_start + segment.virtual_address - USER_IMAGE_VA,
            ) as *mut u8;
            core::ptr::copy_nonoverlapping(source, destination, segment.file_size);
        }
    }

    Ok(LoadedImage {
        entry,
        upper_bound: image_end,
        physical_start,
        page_count,
        kinds,
    })
}

/// Short diagnostic name for an [`ElfError`], for kernel log lines.
pub fn error_name(error: ElfError) -> &'static str {
    match error {
        ElfError::BadMagic => "bad magic",
        ElfError::BadClass => "bad class",
        ElfError::BadDataEncoding => "bad data encoding",
        ElfError::NotExecutable => "not ET_EXEC",
        ElfError::WrongMachine => "wrong machine",
        ElfError::Truncated => "truncated",
        ElfError::NoProgramHeaders => "no program headers",
        ElfError::BadProgramHeader => "bad program header",
        ElfError::WritableExecutable => "writable+executable segment",
        ElfError::NoReadPermission => "unreadable segment",
        ElfError::FileLargerThanMemory => "filesz > memsz",
        ElfError::SegmentOutOfRange => "segment out of range",
        ElfError::NoLoadSegments => "no load segments",
        ElfError::EntryOutsideText => "entry outside text",
        ElfError::ImageTooLarge => "image too large",
        ElfError::OverlappingKinds => "overlapping segments",
        ElfError::HoleInImage => "hole in image",
        ElfError::OutOfMemory => "out of memory",
    }
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], ElfError> {
    let end = offset.checked_add(N).ok_or(ElfError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(ElfError::Truncated)?;
    slice.try_into().map_err(|_| ElfError::Truncated)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}
