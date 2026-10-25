//! Separately compiled userspace images, produced by build.rs.

pub const DEMO: u64 = 0;
pub const NAMESERVER: u64 = 1;
pub const SILVERSHELL: u64 = 2;
pub const LS: u64 = 3;
pub const CAT: u64 = 4;
pub const ECHO: u64 = 5;
pub const SAY: u64 = 6;
pub const SL: u64 = 7;
pub const UART_DRIVER: u64 = 8;

static DEMO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_userspace.elf"));
static NAMESERVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silver_nameserver.elf"));
static SILVERSHELL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/silvershell.elf"));
static LS_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ls.elf"));
static CAT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cat.elf"));
static ECHO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/echo.elf"));
static SAY_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/say.elf"));
static SL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sl.elf"));
static UART_DRIVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/uart_driver.elf"));

/// Selects an executable for spawn. Unknown program IDs are rejected.
pub fn image(program: u64) -> Option<&'static [u8]> {
    match program {
        DEMO => Some(DEMO_ELF),
        NAMESERVER => Some(NAMESERVER_ELF),
        SILVERSHELL => Some(SILVERSHELL_ELF),
        LS => Some(LS_ELF),
        CAT => Some(CAT_ELF),
        ECHO => Some(ECHO_ELF),
        SAY => Some(SAY_ELF),
        SL => Some(SL_ELF),
        UART_DRIVER => Some(UART_DRIVER_ELF),
        _ => None,
    }
}
