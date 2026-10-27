#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SCOUT_READY_TIMEOUT_TICKS: u64 = 200;
const SCOUT_POLL_TICKS: u64 = 20;
const SFS_READY_TIMEOUT_TICKS: u64 = 5000;
const SFS_POLL_TICKS: u64 = 50;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("SILVERD: BOOTING\r\n");

    // 1. Scout
    let scout_id = syscall::spawn(syscall::PROGRAM_SCOUT, syscall::task_id())
        .unwrap_or_else(|| fail("SILVERD: SCOUT SPAWN FAILED\r\n"));
    if !syscall::cap_grant(scout_id, scout_id, syscall::CAP_MMIO) {
        fail("SILVERD: SCOUT CAP GRANT FAILED\r\n");
    }
    if !await_scout_ready(scout_id) {
        syscall::print("SILVERD: SCOUT READY TIMEOUT, CONTINUING\r\n");
    }

    // 2. Block driver
    let block_id = syscall::spawn(syscall::PROGRAM_BLOCK_DRIVER, syscall::task_id())
        .unwrap_or_else(|| fail("SILVERD: BLOCK DRIVER SPAWN FAILED\r\n"));
    if !syscall::cap_grant(block_id, block_id, syscall::CAP_MMIO | syscall::CAP_PCI) {
        fail("SILVERD: BLOCK DRIVER CAP GRANT FAILED\r\n");
    }

    // 3. Filesystem server
    let sfs_id = syscall::spawn(syscall::PROGRAM_SFS_SERVER, syscall::task_id())
        .unwrap_or_else(|| fail("SILVERD: SFS SERVER SPAWN FAILED\r\n"));
    if !syscall::cap_grant(sfs_id, sfs_id, syscall::CAP_MMIO) {
        fail("SILVERD: SFS SERVER CAP GRANT FAILED\r\n");
    }
    if !await_sfs_ready(sfs_id) {
        syscall::print("SILVERD: SFS SERVER NOT READY, CONTINUING\r\n");
    }

    // 4. Howl (shell)
    let howl_id = syscall::spawn(syscall::PROGRAM_HOWL, syscall::task_id())
        .unwrap_or_else(|| fail("SILVERD: HOWL SPAWN FAILED\r\n"));
    if !syscall::cap_grant(howl_id, howl_id, syscall::CAP_MMIO) {
        fail("SILVERD: HOWL CAP GRANT FAILED\r\n");
    }
    if !syscall::console_set(howl_id) {
        fail("SILVERD: CONSOLE HANDOFF FAILED\r\n");
    }

    // 5. Howler (UART)
    let howler_id = syscall::spawn(syscall::PROGRAM_HOWLER, howl_id)
        .unwrap_or_else(|| fail("SILVERD: HOWLER SPAWN FAILED\r\n"));
    if !syscall::cap_grant(howler_id, howler_id, syscall::CAP_MMIO | syscall::CAP_IRQ) {
        fail("SILVERD: HOWLER CAP GRANT FAILED\r\n");
    }

    loop {
        // Mailbox reading loop to check for SFS errors
        let mut msg = ipc::Message::new(0, &[]);
        while syscall::receive_timeout(&mut msg, 0).is_ok() {
            if msg.text() == b"SFS ERROR" {
                syscall::print("SILVERD GOT SFS ERROR!\r\n");
            }
        }

        // Sleep 1 second between checks
        syscall::sleep(1000);
    }
}

/// Waits for Scout's `SCOUT READY` handshake.
fn await_scout_ready(scout_id: u64) -> bool {
    let mut remaining = SCOUT_READY_TIMEOUT_TICKS;
    while remaining > 0 {
        if !syscall::task_alive(scout_id) {
            return false;
        }
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive_timeout(&mut message, SCOUT_POLL_TICKS).is_ok()
            && message.sender == scout_id
            && message.text() == b"SCOUT READY" {
            return true;
        }
        remaining = remaining.saturating_sub(SCOUT_POLL_TICKS);
    }
    false
}

/// Waits for the SFS server to report that it has mounted the disk.
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
            if message.text() == b"SFS READY" {
                return true;
            }
            // The server announces a failed mount before it exits, so this is
            // the one place the reason can still be seen. Without decoding it
            // the report would be dropped and only the exit status would remain.
            if message.text() == b"SFS ERROR" {
                report_sfs_error(&message);
                return false;
            }
        }
        remaining = remaining.saturating_sub(SFS_POLL_TICKS);
    }
    false
}

/// Decodes the mount failure the filesystem server forwards.
///
/// Layout is documented in `sfs_server::announce`: at offset 10 the packed
/// summary (reason code, attempt counter, successful reads, driver status),
/// then the bytes read back from sector 0 and the driver's own probe words.
///
/// `print` only reaches the UART while this task owns the console, which is not
/// the case in the default build; the same report is also encoded in the exit
/// status of the server, and the kernel always prints that.
fn report_sfs_error(message: &ipc::Message) {
    let mut summary_bytes = [0_u8; 8];
    summary_bytes.copy_from_slice(&message.payload[10..18]);
    let summary = u64::from_le_bytes(summary_bytes);
    let code = summary & 0xFF;
    syscall::print("SILVERD: SFS MOUNT FAILED code=");
    syscall::print_dec_u64(code);
    syscall::print(" attempts=");
    syscall::print_dec_u64((summary >> 8) & 0xFF);
    syscall::print(" reads=");
    syscall::print_dec_u64((summary >> 16) & 0xFF);
    syscall::print(" driver_status=");
    syscall::print_dec_u64((summary >> 32) & 0xFFFF);
    syscall::print("\r\n");
    syscall::print("SILVERD: reason ");
    syscall::print(match code {
        1 => "1 = block driver missing or rejected the request\r\n",
        2 => "2 = driver answered but the reply was too short\r\n",
        3 => "3 = mapping the DMA window failed (missing CAP_MMIO?)\r\n",
        4 => "4 = sector 0 was read but has no SFS magic\r\n",
        _ => "unknown\r\n",
    });
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