//! Separately compiled userspace image, produced by build.rs.

pub static DEMO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_userspace.elf"));
