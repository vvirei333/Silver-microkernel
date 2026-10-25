//! Separately compiled userspace images, produced by build.rs.

pub const DEMO: u64 = 0;
pub const NAMESERVER: u64 = 1;
pub const SILVERSHELL: u64 = 2;
pub const LS: u64 = 3;
pub const CAT: u64 = 4;
pub const ECHO: u64 = 5;

static DEMO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_userspace.elf"));
static NAMESERVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_nameserver.elf"));
static SILVERSHELL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silvershell.elf"));
static LS_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ls.elf"));
static CAT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cat.elf"));
static ECHO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/echo.elf"));

/// Selects an executable for spawn. Unknown program IDs are rejected.
pub fn image(program: u64) -> Option<&'static [u8]> {
    match program {
        DEMO => Some(DEMO_ELF),
        NAMESERVER => Some(NAMESERVER_ELF),
        SILVERSHELL => Some(SILVERSHELL_ELF),
        LS => Some(LS_ELF),
        CAT => Some(CAT_ELF),
        ECHO => Some(ECHO_ELF),
        _ => None,
    }
}
