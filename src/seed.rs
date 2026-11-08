//! Initramfs-style bootstrap files seeded into the VFS at boot.
//!
//! These are byte blobs, not programs: `hello.elf` is built by `build.rs`
//! alongside every other userspace binary but is deliberately absent from
//! `user_program::image`. The only way to run it is the dynamic loader
//! (`run bin/hello`), which reads it back out of the VFS and ELF-loads it —
//! proving the loader path without embedding the program in the kernel.

/// The `hello` EL0 (userspace) program, loaded dynamically by `run bin/hello`.
pub const HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));
/// Filesystem sandbox probe: `run /bin/writeprobe` must fail to write `/sfs`.
pub const WRITEPROBE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/writeprobe.elf"));
/// Register-preservation probe: `run /bin/regprobe` (status 0 = all kept).
pub const REGPROBE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/regprobe.elf"));
/// Manifest-scope probe: installed under `/sfs/apps` with a write grant.
pub const SCOPEPROBE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scopeprobe.elf"));
/// The write prefix the probe's manifest grants. The manifest is generated at
/// boot so its `sha256=` line always covers the exact embedded ELF bytes.
/// The `crash` EL0 (userspace) program, loaded dynamically by `run bin/crash`. It writes
/// to `0x0` so the EL0 fault handler has a deterministic way to be exercised.
pub const CRASH_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crash.elf"));
/// Built-in apps, installed into `/sfs` and launched from there by SilverHome.

/// Installs the built-in app bundles into the mounted SFS. Called after
/// `/sfs` is mounted; a file whose size already matches is left untouched.
pub fn seed_apps() {
    // One directory per install root. `mkdir` fails when it already exists,
    // which is the normal case after the first boot; a genuinely broken path
    // shows up as a failed install below.
    let _ = crate::vfs::mkdir("/", "/sfs/apps");
    for (path, image) in [
        ("/sfs/apps/scopeprobe.elf", SCOPEPROBE_ELF),
    ] {
        match crate::vfs::file_len("/", path) {
            Ok(length) if length == image.len() => continue,
            _ => {}
        }
        if crate::vfs::seed_file("/", path, image).is_err() {
            crate::console::write_str("SEED: failed to install app into /sfs\r\n");
        }
    }

    // The manifest is regenerated so the digest always matches the embedded
    // probe. A stale manifest would make `run` fail closed, which is visible
    // immediately and safer than trusting an old hash.
    use core::fmt::Write as _;
    let digest = silver_microkernel::sha256::digest(SCOPEPROBE_ELF);
    let mut manifest = alloc::string::String::from(
        "# scopeprobe: scoped writes, quota and integrity\nwrite=/sfs/probe-scope\nquota=8:5\nsha256=",
    );
    for byte in digest {
        let _ = write!(&mut manifest, "{byte:02x}");
    }
    manifest.push('\n');
    if crate::vfs::seed_file("/", "/sfs/apps/scopeprobe.manifest", manifest.as_bytes()).is_err() {
        crate::console::write_str("SEED: failed to install scopeprobe manifest\r\n");
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
    if crate::vfs::seed_file("/", "/bin/regprobe", REGPROBE_ELF).is_err() {
        crate::console::write_str("SEED: failed to seed /bin/regprobe\r\n");
    }
    if crate::vfs::seed_file("/", "/bin/writeprobe", WRITEPROBE_ELF).is_err() {
        crate::console::write_str("SEED: failed to seed /bin/writeprobe\r\n");
    }
}
