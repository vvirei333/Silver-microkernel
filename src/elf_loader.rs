//! Embedded-image loader.
//!
//! Phase 2 unified this module with the dynamic [`crate::elf`] loader: the
//! image type is now [`crate::elf::LoadedImage`] everywhere, and [`load`] is a
//! thin wrapper over [`crate::elf::load_elf`]. The embedded program table
//! (`user_program::image`) keeps feeding byte blobs here, while the dynamic
//! `run <path>` path reads bytes from the VFS and calls [`crate::elf::load_elf`]
//! directly so it can surface the structured [`crate::elf::ElfError`].

use crate::{elf, console};

/// Loads an embedded userspace image from `user_program::image`.
///
/// On rejection the reason is logged to the kernel UART (`console` writes are
/// visible for every task, unlike `syscall::print`), then `None` is returned.
pub fn load(bytes: &[u8]) -> Option<elf::LoadedImage> {
    match elf::load_elf(bytes) {
        Ok(image) => Some(image),
        Err(error) => {
            console::write_str("ELF: reject - ");
            console::write_str(elf::error_name(error));
            console::write_str("\r\n");
            None
        }
    }
}
