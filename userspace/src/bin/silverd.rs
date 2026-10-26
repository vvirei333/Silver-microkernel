#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SCOUT_READY_TIMEOUT_TICKS: u64 = 200;
const SCOUT_POLL_TICKS: u64 = 20;
/// The filesystem server may retry its mount for a couple of seconds while the
/// block driver finishes device setup, so it gets a generous but finite window.
const SFS_READY_TIMEOUT_TICKS: u64 = 500;
const SFS_POLL_TICKS: u64 = 50;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("SILVERD: BOOTING\r\n");

    // 1. Scout: the nameserver must exist before any other task wants to
    //    register a name. Wait for its readiness handshake, but never block the
    //    boot forever if it fails.
    let (scout_cap, scout_id) = spawn(syscall::PROGRAM_SCOUT, syscall::task_id());
    if !syscall::cap_grant(scout_cap, scout_id, syscall::CAP_MMIO) {
        fail("SILVERD: SCOUT CAP GRANT FAILED\r\n");
    }
    if !await_scout_ready(scout_id) {
        syscall::print("SILVERD: SCOUT READY TIMEOUT, CONTINUING\r\n");
    }

    // 2. Block driver: PCI + MMIO only. Its task ID is looked up by program ID
    //    everywhere it is needed, never hardcoded at the call sites.
    let (block_cap, block_id) = spawn(syscall::PROGRAM_BLOCK_DRIVER, syscall::task_id());
    if !syscall::cap_grant(block_cap, block_id, syscall::CAP_MMIO | syscall::CAP_PCI) {
        fail("SILVERD: BLOCK DRIVER CAP GRANT FAILED\r\n");
    }

    // 3. Filesystem server: reads the SFS superblock through the block driver
    //    and answers metadata queries. It needs MMIO only for the shared DMA
    //    window `map_phys` hands back. Boot waits for its handshake so a failed
    //    mount is reported before the shell takes over the console.
    let (sfs_cap, sfs_id) = spawn(syscall::PROGRAM_SFS_SERVER, syscall::task_id());
    if !syscall::cap_grant(sfs_cap, sfs_id, syscall::CAP_MMIO) {
        fail("SILVERD: SFS SERVER CAP GRANT FAILED\r\n");
    }
    if !await_sfs_ready(sfs_id) {
        syscall::print("SILVERD: SFS SERVER NOT READY, CONTINUING\r\n");
    }

    // 4. Howl (shell) takes the console. Howler does not exist yet, so Howl gets
    //    silverd's own ID as a harmless bootstrap hint and resolves Howler from
    //    the task table when it needs it.
    let (howl_cap, howl_id) = spawn(syscall::PROGRAM_HOWL, syscall::task_id());
    if !syscall::cap_grant(howl_cap, howl_id, syscall::CAP_MMIO) {
        fail("SILVERD: HOWL CAP GRANT FAILED\r\n");
    }
    if !syscall::console_set(howl_cap) {
        fail("SILVERD: CONSOLE HANDOFF FAILED\r\n");
    }

    // 5. Howler (UART) reads the console and feeds Howl's mailbox.
    let (howler_cap, howler_id) = spawn(syscall::PROGRAM_HOWLER, howl_id);
    if !syscall::cap_grant(howler_cap, howler_id, syscall::CAP_MMIO | syscall::CAP_IRQ) {
        fail("SILVERD: HOWLER CAP GRANT FAILED\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

/// Waits for Scout's `SCOUT READY` handshake without depending on any task ID
/// being known in advance, and without blocking forever on a dead Scout.
fn await_scout_ready(scout_id: u64) -> bool {
    let mut remaining = SCOUT_READY_TIMEOUT_TICKS;
    while remaining > 0 {
        if !syscall::task_alive(scout_id) {
            return false;
        }
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive_timeout(&mut message, SCOUT_POLL_TICKS).is_ok()
            && message.sender == scout_id
            && message.text() == b"SCOUT READY"
        {
            return true;
        }
        remaining = remaining.saturating_sub(SCOUT_POLL_TICKS);
    }
    false
}

/// Waits for the filesystem server's mount handshake without depending on any
/// task ID being known in advance, and without blocking forever if it crashed.
fn await_sfs_ready(sfs_id: u64) -> bool {
    let mut remaining = SFS_READY_TIMEOUT_TICKS;
    while remaining > 0 {
        if !syscall::task_alive(sfs_id) {
            return false;
        }
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive_timeout(&mut message, SFS_POLL_TICKS).is_ok()
            && message.sender == sfs_id
        {
            match message.text() {
                b"SFS READY" => return true,
                // The server reports the reason as a code after the string so
                // the boot log names the cause without the server owning the
                // console. See MountError in sfs_server.rs.
                b"SFS ERROR" => {
                    let mut summary_bytes = [0_u8; 8];
                    let mut leading_bytes = [0_u8; 8];
                    let mut physical_bytes = [0_u8; 8];
                    summary_bytes.copy_from_slice(&message.payload[10..18]);
                    leading_bytes.copy_from_slice(&message.payload[18..26]);
                    physical_bytes[..6].copy_from_slice(&message.payload[26..32]);
                    syscall::print("SILVERD: SFS MOUNT FAILED, SUMMARY 0x");
                    syscall::print_hex_u64(u64::from_le_bytes(summary_bytes));
                    syscall::print(" FIRST 0x");
                    syscall::print_hex_u64(u64::from_le_bytes(leading_bytes));
                    syscall::print(" PADDR 0x");
                    syscall::print_hex_u64(u64::from_le_bytes(physical_bytes));
                    syscall::print("\r\n");
                    return false;
                }
                _ => {}
            }
        }
        remaining = remaining.saturating_sub(SFS_POLL_TICKS);
    }
    false
}

fn spawn(program: u64, argument: u64) -> (u64, u64) {
    let Some(cap_id) = syscall::spawn(program, argument) else {
        fail("SILVERD: SPAWN FAILED\r\n");
    };
    let Some(task_id) = syscall::cap_get_task_id(cap_id) else {
        fail("SILVERD: CAP RESOLVE FAILED\r\n");
    };
    (cap_id, task_id)
}

fn fail(message: &str) -> ! {
    syscall::print(message);
    syscall::exit_with_status(1)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
