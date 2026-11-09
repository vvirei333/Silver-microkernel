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
            return Err(UserFault);
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

pub fn copy_to_user(source: &[u8], destination: u64) -> Result<(), UserFault> {
    let mut done = 0usize;
    while done < source.len() {
        let take = (source.len() - done).min(COPY_CHUNK);
        let window = &source[done..done + take];
        if !mmu::valid_user_slice_mut(destination + done as u64, take as u64) {
            return Err(UserFault);
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
