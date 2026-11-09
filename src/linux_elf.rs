//! Loader for foreign (Linux ABI) AArch64 ELF images.
//!
//! Unlike [`crate::elf::load_elf`], which serves the Silver userspace layout
//! (three segments packed into one contiguous run at `USER_IMAGE_VA`), this
//! loader honours the file's own `p_vaddr`: a normal Linux binary links at
//! `0x40_0000` with separate text/data segments and gaps between them. Each
//! `PT_LOAD` segment gets its own physical run; the runs are mapped into the
//! child's address space at their virtual addresses by
//! [`crate::mmu::install_linux_image`].
//!
//! Validation is complete before any frame is allocated, and every run is
//! owned by the returned [`LinuxImage`], whose `Drop` releases it. The loader
//! never maps anything: the address space does not exist yet.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    elf::{self, Elf64Ehdr, Elf64Phdr, PageKind},
    frame_allocator,
    memory::{phys_to_virt, PAGE_SIZE},
    mmu,
};

/// Upper bound on one image, in pages (32 MiB). The main executable streams
/// from the VFS, so this is the real frame budget rather than a whole-file
/// buffer cap; it still rejects a crafted `p_memsz`.
const MAX_IMAGE_PAGES: usize = 8192;
/// A normal ELF has a handful of `PT_LOAD` segments; more is suspicious.
const MAX_SEGMENTS: usize = 16;

/// Why a foreign image was rejected.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LinuxElfError {
    BadMagic,
    BadClass,
    BadDataEncoding,
    NotExecutable,
    WrongMachine,
    Truncated,
    NoProgramHeaders,
    BadProgramHeader,
    NoLoadSegments,
    WritableExecutable,
    NoReadPermission,
    FileLargerThanMemory,
    SegmentOutOfRange,
    EntryOutsideText,
    OverlappingSegments,
    ImageTooLarge,
    OutOfMemory,
    /// `PT_INTERP` names a file that does not exist in the VFS.
    InterpreterMissing,
}

/// One contiguous physical run mapped at `virtual_address`.
pub struct LinuxRun {
    /// Page-aligned start of the run in the child's virtual space.
    pub virtual_address: usize,
    pub physical_start: usize,
    pub page_count: usize,
    pub generation: u32,
    /// Permission class per page, index `0` is `virtual_address`.
    pub kinds: Vec<PageKind>,
}

/// A fully loaded Linux image plus the metadata the scheduler needs to map it
/// and release its frames.
pub struct LinuxImage {
    /// Address the kernel starts at: the interpreter entry when one is
    /// attached, otherwise the program entry.
    pub entry: usize,
    /// The main executable's entry (`AT_ENTRY`), even when an interpreter is
    /// attached.
    pub program_entry: usize,
    /// Load bias of the interpreter (`AT_BASE`), or 0.
    pub interp_base: usize,
    /// `PT_INTERP` path, once parsed.
    pub interp_path: Option<alloc::string::String>,
    pub phdr_address: usize,
    pub phentsize: u64,
    pub phnum: u64,
    /// Highest `p_vaddr + p_memsz` in the image.
    pub upper_bound: usize,
    pub runs: Vec<LinuxRun>,
    released: AtomicBool,
}

impl LinuxImage {
    /// Total frames held by the image, for the per-task memory ledger.
    pub fn page_count(&self) -> usize {
        self.runs.iter().map(|run| run.page_count).sum()
    }

    /// Deep copy for `fork`: fresh runs with the same content, addresses and
    /// permission classes.
    pub fn duplicate(&self) -> Option<LinuxImage> {
        let mut runs: Vec<LinuxRun> = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            let Some((physical_start, generation)) =
                frame_allocator::alloc_user_contiguous_pages_tagged(run.page_count)
            else {
                for prior in &runs {
                    frame_allocator::free_contiguous_pages_tagged(
                        prior.physical_start,
                        prior.page_count,
                        prior.generation,
                    );
                }
                return None;
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(run.physical_start) as *const u8,
                    phys_to_virt(physical_start) as *mut u8,
                    run.page_count * PAGE_SIZE,
                );
            }
            runs.push(LinuxRun {
                virtual_address: run.virtual_address,
                physical_start,
                page_count: run.page_count,
                generation,
                kinds: run.kinds.clone(),
            });
        }
        Some(LinuxImage {
            entry: self.entry,
            program_entry: self.program_entry,
            interp_base: self.interp_base,
            interp_path: self.interp_path.clone(),
            phdr_address: self.phdr_address,
            phentsize: self.phentsize,
            phnum: self.phnum,
            upper_bound: self.upper_bound,
            runs,
            released: AtomicBool::new(false),
        })
    }

    /// Shares every run with a forked child. The refcount is bumped for each
    /// page and writable data pages become `PageKind::Cow`, so both address
    /// spaces map them read-only until a write fault copies the run.
    pub fn share(&self) -> Option<LinuxImage> {
        let mut runs: Vec<LinuxRun> = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            if !frame_allocator::share_contiguous_pages_tagged(
                run.physical_start,
                run.page_count,
                run.generation,
            ) {
                for prior in &runs {
                    frame_allocator::free_contiguous_pages_tagged(
                        prior.physical_start,
                        prior.page_count,
                        prior.generation,
                    );
                }
                return None;
            }
            let mut kinds = run.kinds.clone();
            for kind in &mut kinds {
                if *kind == PageKind::Data {
                    *kind = PageKind::Cow;
                }
            }
            runs.push(LinuxRun {
                virtual_address: run.virtual_address,
                physical_start: run.physical_start,
                page_count: run.page_count,
                generation: run.generation,
                kinds,
            });
        }
        Some(LinuxImage {
            entry: self.entry,
            program_entry: self.program_entry,
            interp_base: self.interp_base,
            interp_path: self.interp_path.clone(),
            phdr_address: self.phdr_address,
            phentsize: self.phentsize,
            phnum: self.phnum,
            upper_bound: self.upper_bound,
            runs,
            released: AtomicBool::new(false),
        })
    }

    /// Converts writable image pages to the shared COW state. Called on the
    /// parent after `share`; the parent's PTEs are made read-only separately.
    pub fn mark_data_cow(&mut self) {
        for run in &mut self.runs {
            for kind in &mut run.kinds {
                if *kind == PageKind::Data {
                    *kind = PageKind::Cow;
                }
            }
        }
    }

    /// Attaches an already-loaded interpreter image at `base` (the `AT_BASE`
    /// value) and switches the start address to the interpreter entry.
    pub fn attach_interpreter(&mut self, mut interp: LinuxImage, base: usize) -> bool {
        if interp.interp_path.is_some() || interp.interp_base != 0 {
            return false;
        }
        for run in &interp.runs {
            let start = run.virtual_address;
            let end = start + run.page_count * PAGE_SIZE;
            for existing in &self.runs {
                let existing_start = existing.virtual_address;
                let existing_end = existing_start + existing.page_count * PAGE_SIZE;
                if start < existing_end && existing_start < end {
                    return false;
                }
            }
        }
        self.entry = interp.entry;
        self.interp_base = base;
        self.upper_bound = self.upper_bound.max(interp.upper_bound);
        self.runs.append(&mut interp.runs);
        // The empty shell must not release anything on drop.
        interp.released.store(true, Ordering::Release);
        true
    }

    /// Returns every run to the allocator. Idempotent, like
    /// [`crate::elf::LoadedImage::release`].
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        for run in &self.runs {
            frame_allocator::free_contiguous_pages_tagged(
                run.physical_start,
                run.page_count,
                run.generation,
            );
        }
    }
}

impl Drop for LinuxImage {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn error_name(error: LinuxElfError) -> &'static str {
    match error {
        LinuxElfError::BadMagic => "bad magic",
        LinuxElfError::BadClass => "bad class",
        LinuxElfError::BadDataEncoding => "bad data encoding",
        LinuxElfError::NotExecutable => "not ET_EXEC",
        LinuxElfError::WrongMachine => "wrong machine",
        LinuxElfError::Truncated => "truncated",
        LinuxElfError::NoProgramHeaders => "no program headers",
        LinuxElfError::BadProgramHeader => "bad program header",
        LinuxElfError::NoLoadSegments => "no load segments",
        LinuxElfError::WritableExecutable => "writable+executable segment",
        LinuxElfError::NoReadPermission => "unreadable segment",
        LinuxElfError::FileLargerThanMemory => "filesz > memsz",
        LinuxElfError::SegmentOutOfRange => "segment out of range",
        LinuxElfError::EntryOutsideText => "entry outside text",
        LinuxElfError::OverlappingSegments => "overlapping segments",
        LinuxElfError::ImageTooLarge => "image too large",
        LinuxElfError::OutOfMemory => "out of memory",
        LinuxElfError::InterpreterMissing => "interpreter missing",
    }
}

struct Segment {
    file_offset: usize,
    virtual_address: usize,
    file_size: usize,
    memory_size: usize,
    kind: PageKind,
}

/// Parses, validates, allocates and fills a Linux ABI image.
pub fn load_linux_elf(bytes: &[u8]) -> Result<LinuxImage, LinuxElfError> {
    load_linux_elf_at(bytes, 0)
}

/// Preferred bases for PIE images: the main program and its interpreter must
/// not overlap, and both must stay below the fixed user-stack window.
pub const MAIN_BIAS: usize = 0x1000_0000;
pub const INTERP_BIAS: usize = 0x2000_0000;

/// Loads a program the way `execve` does: chooses a bias for `ET_DYN`, then
/// resolves and attaches a `PT_INTERP` interpreter from the VFS.
pub fn load_linux_program(
    cwd: &str,
    bytes: &[u8],
    limit: usize,
) -> Result<LinuxImage, LinuxElfError> {
    let e_type = u16::from_le_bytes(
        bytes
            .get(16..18)
            .ok_or(LinuxElfError::Truncated)?
            .try_into()
            .map_err(|_| LinuxElfError::Truncated)?,
    );
    let bias = if e_type == elf::ET_DYN {
        MAIN_BIAS
    } else {
        0
    };
    let mut image = load_linux_elf_at(bytes, bias)?;
    if let Some(path) = image.interp_path.clone() {
        attach_interpreter(&mut image, cwd, &path, limit)?;
    }
    Ok(image)
}

/// Streams a main executable from the VFS: only the ELF header, program
/// headers and interpreter are buffered, while `PT_LOAD` bytes are copied
/// chunk by chunk into their frames. The whole-file 8 MiB exec limit no longer
/// applies to the main program.
pub fn load_linux_program_path(
    cwd: &str,
    path: &str,
    limit: usize,
) -> Result<LinuxImage, LinuxElfError> {
    let length = crate::vfs::file_len(cwd, path)
        .map_err(|_| LinuxElfError::InterpreterMissing)?;
    if length > limit {
        return Err(LinuxElfError::ImageTooLarge);
    }
    let source = VfsSource {
        cwd,
        path,
        length,
    };
    let mut header = [0_u8; 64];
    if !source.read(0, &mut header) {
        return Err(LinuxElfError::Truncated);
    }
    let e_type = u16::from_le_bytes([header[16], header[17]]);
    let bias = if e_type == elf::ET_DYN { MAIN_BIAS } else { 0 };
    let mut image = load_linux_elf_from_source(&source, bias)?;
    if let Some(interp_path) = image.interp_path.clone() {
        attach_interpreter(&mut image, cwd, &interp_path, crate::vfs::MAX_FILE_BYTES)?;
    }
    Ok(image)
}

fn attach_interpreter(
    image: &mut LinuxImage,
    cwd: &str,
    path: &str,
    limit: usize,
) -> Result<(), LinuxElfError> {
    let length = crate::vfs::file_len(cwd, path).map_err(|_| LinuxElfError::InterpreterMissing)?;
    if length > limit {
        return Err(LinuxElfError::ImageTooLarge);
    }
    let bytes = crate::vfs::read_file_limit(cwd, path, limit)
        .map_err(|_| LinuxElfError::InterpreterMissing)?;
    let interp = load_linux_elf_at(&bytes, INTERP_BIAS)?;
    if !image.attach_interpreter(interp, INTERP_BIAS) {
        return Err(LinuxElfError::OverlappingSegments);
    }
    Ok(())
}

/// Byte source for the ELF loader. The slice implementation keeps the host
/// tests and the small interpreter path allocation-free; the VFS implementation
/// lets a large main executable stream segment bytes into its final frames.
trait ElfSource {
    fn len(&self) -> usize;
    /// Reads exactly `out.len()` bytes at `offset`; false on any short read.
    fn read(&self, offset: usize, out: &mut [u8]) -> bool;
}

struct SliceSource<'a>(&'a [u8]);

impl ElfSource for SliceSource<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(out.len()) else {
            return false;
        };
        let Some(bytes) = self.0.get(offset..end) else {
            return false;
        };
        out.copy_from_slice(bytes);
        true
    }
}

struct VfsSource<'a> {
    cwd: &'a str,
    path: &'a str,
    length: usize,
}

impl ElfSource for VfsSource<'_> {
    fn len(&self) -> usize {
        self.length
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        match crate::vfs::read_at(self.cwd, self.path, offset, out.len()) {
            Ok(bytes) if bytes.len() == out.len() => {
                out.copy_from_slice(&bytes);
                true
            }
            _ => false,
        }
    }
}

/// Loads a foreign image at `bias`. `ET_EXEC` images use 0; `ET_DYN`/PIE
/// images are placed at a non-zero base chosen by the caller.
pub fn load_linux_elf_at(bytes: &[u8], bias: usize) -> Result<LinuxImage, LinuxElfError> {
    load_linux_elf_from_source(&SliceSource(bytes), bias)
}

fn load_linux_elf_from_source(
    source: &dyn ElfSource,
    bias: usize,
) -> Result<LinuxImage, LinuxElfError> {
    let mut header_bytes = [0_u8; 64];
    if !source.read(0, &mut header_bytes) {
        return Err(LinuxElfError::Truncated);
    }
    let header = Elf64Ehdr::parse(&header_bytes).map_err(map_error)?;
    if &header.ident[..4] != b"\x7FELF" {
        return Err(LinuxElfError::BadMagic);
    }
    if header.ident[4] != 2 {
        return Err(LinuxElfError::BadClass);
    }
    if header.ident[5] != 1 {
        return Err(LinuxElfError::BadDataEncoding);
    }
    if header.e_type != elf::ET_EXEC && header.e_type != elf::ET_DYN {
        return Err(LinuxElfError::NotExecutable);
    }
    if header.e_machine != elf::EM_AARCH64 {
        return Err(LinuxElfError::WrongMachine);
    }

    let entry = (header.e_entry as usize)
        .checked_add(bias)
        .ok_or(LinuxElfError::SegmentOutOfRange)?;
    let program_offset = header.e_phoff as usize;
    let program_size = header.e_phentsize as usize;
    let program_count = header.e_phnum as usize;
    if program_size < elf::PHDR_SIZE || program_count == 0 {
        return Err(LinuxElfError::NoProgramHeaders);
    }
    if program_count > MAX_SEGMENTS {
        return Err(LinuxElfError::ImageTooLarge);
    }

    let mut segments = Vec::new();
    let mut total_pages = 0usize;
    let mut interp_path: Option<alloc::string::String> = None;
    for index in 0..program_count {
        let header_offset = program_offset
            .checked_add(index.checked_mul(program_size).ok_or(LinuxElfError::Truncated)?)
            .ok_or(LinuxElfError::Truncated)?;
        let mut phdr_bytes = [0_u8; elf::PHDR_SIZE];
        if !source.read(header_offset, &mut phdr_bytes) {
            return Err(LinuxElfError::Truncated);
        }
        let ph = Elf64Phdr::parse(&phdr_bytes, 0).map_err(map_error)?;
        if ph.p_type == elf::PT_INTERP {
            let start = ph.p_offset as usize;
            let length = ph.p_filesz as usize;
            if length == 0 || length > 256 {
                return Err(LinuxElfError::BadProgramHeader);
            }
            let mut raw = vec![0_u8; length];
            if !source.read(start, &mut raw) {
                return Err(LinuxElfError::Truncated);
            }
            let text = raw.split(|&byte| byte == 0).next().unwrap_or(&[]);
            if text.is_empty() || text.len() > 255 {
                return Err(LinuxElfError::BadProgramHeader);
            }
            let path = core::str::from_utf8(text)
                .map_err(|_| LinuxElfError::BadProgramHeader)?;
            interp_path = Some(alloc::string::String::from(path));
            continue;
        }
        if ph.p_type != elf::PT_LOAD {
            continue;
        }
        let flags = ph.p_flags;
        if flags & elf::PF_W != 0 && flags & elf::PF_X != 0 {
            return Err(LinuxElfError::WritableExecutable);
        }
        if flags & elf::PF_R == 0 {
            return Err(LinuxElfError::NoReadPermission);
        }
        let kind = if flags & elf::PF_X != 0 {
            PageKind::Text
        } else if flags & elf::PF_W != 0 {
            PageKind::Data
        } else {
            PageKind::ReadOnly
        };
        let file_offset = ph.p_offset as usize;
        let virtual_address = (ph.p_vaddr as usize)
            .checked_add(bias)
            .ok_or(LinuxElfError::SegmentOutOfRange)?;
        let file_size = ph.p_filesz as usize;
        let memory_size = ph.p_memsz as usize;
        if memory_size == 0 {
            continue;
        }
        if file_size > memory_size {
            return Err(LinuxElfError::FileLargerThanMemory);
        }
        let memory_end = virtual_address
            .checked_add(memory_size)
            .ok_or(LinuxElfError::SegmentOutOfRange)?;
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(LinuxElfError::SegmentOutOfRange)?;
        if file_end > source.len()
            || memory_end > mmu::USER_STACK_GUARD_VA
            || virtual_address < PAGE_SIZE
        {
            return Err(LinuxElfError::SegmentOutOfRange);
        }
        // The file bytes of a segment must start on the same page offset as
        // the segment itself, otherwise the first partial page cannot be
        // filled from the file.
        if file_offset % PAGE_SIZE != virtual_address % PAGE_SIZE {
            return Err(LinuxElfError::BadProgramHeader);
        }
        let page_start = virtual_address & !(PAGE_SIZE - 1);
        let pages = (memory_end.div_ceil(PAGE_SIZE)) - page_start / PAGE_SIZE;
        total_pages = total_pages
            .checked_add(pages)
            .ok_or(LinuxElfError::ImageTooLarge)?;
        if total_pages > MAX_IMAGE_PAGES {
            return Err(LinuxElfError::ImageTooLarge);
        }
        segments.push(Segment {
            file_offset,
            virtual_address,
            file_size,
            memory_size,
            kind,
        });
    }

    if segments.is_empty() {
        return Err(LinuxElfError::NoLoadSegments);
    }
    if !segments.iter().any(|segment| {
        segment.kind == PageKind::Text
            && entry >= segment.virtual_address
            && entry < segment.virtual_address + segment.memory_size
    }) {
        return Err(LinuxElfError::EntryOutsideText);
    }

    // Sort by address. Two segments may not share a page: their permission
    // classes would conflict and one copy would overwrite the other.
    segments.sort_by_key(|segment| segment.virtual_address);
    let mut previous_end = 0usize;
    for segment in &segments {
        let start = segment.virtual_address & !(PAGE_SIZE - 1);
        if start < previous_end {
            return Err(LinuxElfError::OverlappingSegments);
        }
        previous_end = (segment.virtual_address + segment.memory_size).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    }

    // Compute auxv metadata before allocating anything.
    let phdr_file_end = program_offset
        .checked_add(program_count.checked_mul(program_size).ok_or(LinuxElfError::Truncated)?)
        .ok_or(LinuxElfError::Truncated)?;
    let mut phdr_address = 0usize;
    for segment in &segments {
        let segment_file_end = segment
            .file_offset
            .checked_add(segment.file_size)
            .ok_or(LinuxElfError::SegmentOutOfRange)?;
        if program_offset >= segment.file_offset && phdr_file_end <= segment_file_end {
            phdr_address = segment
                .virtual_address
                .checked_add(program_offset - segment.file_offset)
                .ok_or(LinuxElfError::SegmentOutOfRange)?;
        }
    }

    let mut runs: Vec<LinuxRun> = Vec::with_capacity(segments.len());
    let mut upper_bound = 0usize;
    for segment in &segments {
        let page_start = segment.virtual_address & !(PAGE_SIZE - 1);
        let pages = (segment.virtual_address + segment.memory_size).div_ceil(PAGE_SIZE)
            - page_start / PAGE_SIZE;
        let Some((physical_start, generation)) =
            frame_allocator::alloc_user_contiguous_pages_tagged(pages)
        else {
            // Release the runs allocated so far: they are not yet owned by a
            // `LinuxImage`, so dropping the vector would leak their frames.
            for run in &runs {
                frame_allocator::free_contiguous_pages_tagged(
                    run.physical_start,
                    run.page_count,
                    run.generation,
                );
            }
            return Err(LinuxElfError::OutOfMemory);
        };
        let run = LinuxRun {
            virtual_address: page_start,
            physical_start,
            page_count: pages,
            generation,
            kinds: vec![segment.kind; pages],
        };
        // Zero the whole run first (BSS is the tail the file does not cover),
        // then stream the file bytes from the segment's page offset directly
        // into their final frames. No whole-file buffer is allocated.
        unsafe {
            core::ptr::write_bytes(
                phys_to_virt(physical_start) as *mut u8,
                0,
                pages * PAGE_SIZE,
            );
            let page_offset = segment.virtual_address - page_start;
            let source_start = segment
                .file_offset
                .checked_sub(page_offset)
                .ok_or(LinuxElfError::SegmentOutOfRange)?;
            let copy_len = (page_offset + segment.file_size)
                .min(pages * PAGE_SIZE)
                .min(source.len().saturating_sub(source_start));
            let mut copied = 0usize;
            let mut chunk = [0_u8; 512];
            while copied < copy_len {
                let take = (copy_len - copied).min(chunk.len());
                if !source.read(source_start + copied, &mut chunk[..take]) {
                    for prior in &runs {
                        frame_allocator::free_contiguous_pages_tagged(
                            prior.physical_start,
                            prior.page_count,
                            prior.generation,
                        );
                    }
                    frame_allocator::free_contiguous_pages_tagged(
                        physical_start,
                        pages,
                        generation,
                    );
                    return Err(LinuxElfError::Truncated);
                }
                core::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    (phys_to_virt(physical_start) as *mut u8).add(copied),
                    take,
                );
                copied += take;
            }
        }
        upper_bound = upper_bound.max(segment.virtual_address + segment.memory_size);
        runs.push(run);
    }

    Ok(LinuxImage {
        entry,
        program_entry: entry,
        interp_base: 0,
        interp_path,
        phdr_address,
        phentsize: program_size as u64,
        phnum: program_count as u64,
        upper_bound,
        runs,
        released: AtomicBool::new(false),
    })
}

fn map_error(error: elf::ElfError) -> LinuxElfError {
    match error {
        elf::ElfError::Truncated => LinuxElfError::Truncated,
        elf::ElfError::BadProgramHeader => LinuxElfError::BadProgramHeader,
        _ => LinuxElfError::BadProgramHeader,
    }
}
