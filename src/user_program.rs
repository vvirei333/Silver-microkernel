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
pub const BLOCK_DRIVER: u64 = 10;
pub const SFS_SERVER: u64 = 11; // <--- ДОБАВЛЕНО
pub const NET_DRIVER: u64 = 12;
pub const NET_SERVER: u64 = 13;
pub const GPU_DRIVER: u64 = 14;
pub const INPUT_DRIVER: u64 = 15;

/// Sentinel program ID for tasks spawned from a dynamically loaded ELF (the
/// `run <path>` path). Distinct from `u64::MAX`, which marks kernel tasks.
pub const PROGRAM_DYNAMIC: u64 = u64::MAX - 1;

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
static BLOCK_DRIVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/block_driver.elf"));
static SFS_SERVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sfs_server.elf")); // <--- ДОБАВЛЕНО
static NET_DRIVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/net_driver.elf"));
static NET_SERVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/net_server.elf"));
static GPU_DRIVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gpu_driver.elf"));
static INPUT_DRIVER_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/input_driver.elf"));

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
        BLOCK_DRIVER => Some(BLOCK_DRIVER_ELF),
        SFS_SERVER => Some(SFS_SERVER_ELF), // <--- ДОБАВЛЕНО
        NET_DRIVER => Some(NET_DRIVER_ELF),
        NET_SERVER => Some(NET_SERVER_ELF),
        GPU_DRIVER => Some(GPU_DRIVER_ELF),
        INPUT_DRIVER => Some(INPUT_DRIVER_ELF),
        _ => None,
    }
}
