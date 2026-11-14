//! Checked user-memory accessors.
//!
//! SCTLR_EL1.PAN stays enabled. `ldtrb` / `sttrb` perform the actual accesses
//! with EL0 permissions, while the destination/source kernel buffers remain
//! ordinary high-half pointers. Validation also walks the current TTBR0 so an
//! invalid buffer fails before any blocking IPC operation.

use crate::mmu;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserFault;

/// One validation/copy window. `mmu::valid_user_slice*` refuses a request
/// larger than one page; chunking keeps that bound while still accepting the
/// multi-page buffers a normal `read(2)` uses.
const COPY_CHUNK: usize = 4096;

pub fn copy_from_user(source: u64, destination: &mut [u8]) -> Result<(), UserFault> {
    // `valid_user_slice` deliberately caps one validation pass at a page-sized
    // window. A real `read(2)` may hand us a 64 KiB buffer, so validate and
    // copy in page-sized chunks instead of failing the whole request.
    let mut done = 0usize;
    while done < destination.len() {
        let take = (destination.len() - done).min(COPY_CHUNK);
        let window = &mut destination[done..done + take];
        if !mmu::valid_user_slice(source + done as u64, take as u64) {
            #[cfg(feature = "linux-abi")]
            back_user_pages(source + done as u64, take, false);
            if !mmu::valid_user_slice(source + done as u64, take as u64) {
                return Err(UserFault);
            }
        }
        for (index, byte) in window.iter_mut().enumerate() {
            let value: u8;
            unsafe {
                core::arch::asm!(
                    "ldtrb {value:w}, [{address}]",
                    address = in(reg) (source as usize + done + index),
                    value = out(reg) value,
                    options(nostack, preserves_flags),
                );
            }
            *byte = value;
        }
        done += take;
    }
    Ok(())
}

/// Backs the user pages of `[pointer, pointer + length)`.
///
/// Two things make a page unreadable until the fault handler runs: a region
/// shared with `fork` is write-protected until `cow_fault` copies it, and a
/// lazily mapped region (an anonymous reservation, or a file-backed mapping
/// since libraries are read on demand) has no frame at all. The kernel copy
/// path checks the page table *before* touching memory — `PAN` is on and the
/// accessors use `ldtrb`/`sttrb` — so a syscall whose buffer or path string
/// lives on an untouched page would otherwise fail with `EFAULT` without the
/// hardware ever faulting the page in.
#[cfg(feature = "linux-abi")]
fn back_user_pages(pointer: u64, length: usize, write: bool) {
    let mut page = pointer & !(COPY_CHUNK as u64 - 1);
    let Some(end) = pointer.checked_add(length as u64) else {
        return;
    };
    while page < end {
        let valid = if write {
            mmu::valid_user_slice_mut(page, 1)
        } else {
            mmu::valid_user_slice(page, 1)
        };
        if !valid {
            let esr = if write { 1u64 << 6 } else { 0 };
            let _ = crate::scheduler::cow_fault(page as usize, esr);
        }
        page = page.saturating_add(COPY_CHUNK as u64);
    }
}

pub fn copy_to_user(source: &[u8], destination: u64) -> Result<(), UserFault> {
    let mut done = 0usize;
    while done < source.len() {
        let take = (source.len() - done).min(COPY_CHUNK);
        let window = &source[done..done + take];
        if !mmu::valid_user_slice_mut(destination + done as u64, take as u64) {
            #[cfg(feature = "linux-abi")]
            back_user_pages(destination + done as u64, take, true);
            if !mmu::valid_user_slice_mut(destination + done as u64, take as u64) {
                return Err(UserFault);
            }
        }
        for (index, value) in window.iter().copied().enumerate() {
            unsafe {
                core::arch::asm!(
                    "sttrb {value:w}, [{address}]",
                    address = in(reg) (destination as usize + done + index),
                    value = in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
        }
        done += take;
    }
    Ok(())
}
