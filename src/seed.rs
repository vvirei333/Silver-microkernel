//! Initramfs-style bootstrap files seeded into the VFS at boot.
//!
//! These are byte blobs, not programs: `hello.elf` is built by `build.rs`
//! alongside every other userspace binary but is deliberately absent from
//! `user_program::image`. The only way to run it is the dynamic loader
//! (`run bin/hello`), which reads it back out of the VFS and ELF-loads it —
//! proving the loader path without embedding the program in the kernel.

/// The `hello` EL0 (userspace) program, loaded dynamically by `run bin/hello`.
pub const HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));
/// The `crash` EL0 (userspace) program, loaded dynamically by `run bin/crash`. It writes
/// to `0x0` so the EL0 fault handler has a deterministic way to be exercised.
pub const CRASH_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crash.elf"));
/// Built-in apps, installed into `/sfs` and launched from there by SilverHome.
pub const CALC_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/calc.elf"));
pub const GALLERY_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gallery.elf"));
pub const NOTES_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/notes.elf"));

/// Installs the built-in app bundles into the mounted SFS. Called after
/// `/sfs` is mounted; a file whose size already matches is left untouched.
pub fn seed_apps() {
    for (path, image) in [
        ("/sfs/calc.elf", CALC_ELF),
        ("/sfs/gallery.elf", GALLERY_ELF),
        ("/sfs/notes.elf", NOTES_ELF),
    ] {
        match crate::vfs::file_len("/", path) {
            Ok(length) if length == image.len() => continue,
            _ => {}
        }
        if crate::vfs::seed_file("/", path, image).is_err() {
            crate::console::write_str("SEED: failed to install app into /sfs\r\n");
        }
    }
}

/// Seeds the bootstrap files into the already-initialized VFS.
pub fn seed() {
    if crate::vfs::seed_file("/", "/bin/hello", HELLO_ELF).is_err() {
        crate::console::write_str("SEED: failed to seed /bin/hello\r\n");
    }
    if crate::vfs::seed_file("/", "/bin/crash", CRASH_ELF).is_err() {
        crate::console::write_str("SEED: failed to seed /bin/crash\r\n");
    }
}
