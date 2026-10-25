//! Separately compiled userspace images, produced by build.rs.

pub const DEMO: u64 = 0;
pub const SILVERD: u64 = 1;
pub const SCOUT: u64 = 2;
pub const HOWL: u64 = 3;
pub const HOWLER: u64 = 4;
pub const LS: u64 = 5;
pub const CAT: u64 = 6;
pub const ECHO: u64 = 7;
pub const SAY: u64 = 8;
pub const SL: u64 = 9;

static DEMO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_userspace.elf"));
static SILVERD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silverd.elf"));
static SCOUT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scout.elf"));
static HOWL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silvershell.elf"));
static HOWLER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/uart_driver.elf"));
static LS_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ls.elf"));
static CAT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cat.elf"));
static ECHO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/echo.elf"));
static SAY_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/say.elf"));
static SL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sl.elf"));

/// Selects an executable for spawn. Unknown program IDs are rejected.
pub fn image(program: u64) -> Option<&'static [u8]> {
    match program {
        DEMO => Some(DEMO_ELF),
        SILVERD => Some(SILVERD_ELF),
        SCOUT => Some(SCOUT_ELF),
        HOWL => Some(HOWL_ELF),
        HOWLER => Some(HOWLER_ELF),
        LS => Some(LS_ELF),
        CAT => Some(CAT_ELF),
        ECHO => Some(ECHO_ELF),
        SAY => Some(SAY_ELF),
        SL => Some(SL_ELF),
        _ => None,
    }
}
