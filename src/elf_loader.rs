//! Embedded-image loader.
//!
//! Phase 2 unified this module with the dynamic [`crate::elf`] loader: the
//! image type is now [`crate::elf::LoadedImage`] everywhere, and [`load`] is a
//! thin wrapper over [`crate::elf::load_elf`]. The embedded program table
//! (`user_program::image`) keeps feeding byte blobs here, while the dynamic
//! `run <path>` path reads bytes from the VFS and calls [`crate::elf::load_elf`]
//! directly so it can surface the structured [`crate::elf::ElfError`].

use crate::{elf, pl011};

/// Loads an embedded userspace image from `user_program::image`.
///
/// On rejection the reason is logged to the kernel UART (`pl011` writes are
/// visible for every task, unlike `syscall::print`), then `None` is returned.
pub fn load(bytes: &[u8]) -> Option<elf::LoadedImage> {
    match elf::load_elf(bytes) {
        Ok(image) => Some(image),
        Err(error) => {
            pl011::write_str("ELF: reject - ");
            pl011::write_str(elf::error_name(error));
            pl011::write_str("\r\n");
            None
        }
    }
}
