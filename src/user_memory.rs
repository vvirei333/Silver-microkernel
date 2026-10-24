//! Checked user-memory accessors.
//!
//! SCTLR_EL1.PAN stays enabled. `ldtrb` / `sttrb` perform the actual accesses
//! with EL0 permissions, while the destination/source kernel buffers remain
//! ordinary high-half pointers. Validation also walks the current TTBR0 so an
//! invalid buffer fails before any blocking IPC operation.

use crate::mmu;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserFault;

pub fn copy_from_user(source: u64, destination: &mut [u8]) -> Result<(), UserFault> {
    if !mmu::valid_user_slice(source, destination.len() as u64) {
        return Err(UserFault);
    }
    for (index, byte) in destination.iter_mut().enumerate() {
        let value: u8;
        unsafe {
            core::arch::asm!(
                "ldtrb {value:w}, [{address}]",
                address = in(reg) source as usize + index,
                value = out(reg) value,
                options(nostack, preserves_flags),
            );
        }
        *byte = value;
    }
    Ok(())
}

pub fn copy_to_user(source: &[u8], destination: u64) -> Result<(), UserFault> {
    if !mmu::valid_user_slice_mut(destination, source.len() as u64) {
        return Err(UserFault);
    }
    for (index, value) in source.iter().copied().enumerate() {
        unsafe {
            core::arch::asm!(
                "sttrb {value:w}, [{address}]",
                address = in(reg) destination as usize + index,
                value = in(reg) value,
                options(nostack, preserves_flags),
            );
        }
    }
    Ok(())
}
