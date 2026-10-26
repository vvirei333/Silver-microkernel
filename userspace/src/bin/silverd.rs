#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SCOUT_READY_TIMEOUT_TICKS: u64 = 200;
const SCOUT_POLL_TICKS: u64 = 20;

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

    // 3. Howl (shell) takes the console. Howler does not exist yet, so Howl gets
    //    silverd's own ID as a harmless bootstrap hint and resolves Howler from
    //    the task table when it needs it.
    let (howl_cap, howl_id) = spawn(syscall::PROGRAM_HOWL, syscall::task_id());
    if !syscall::cap_grant(howl_cap, howl_id, syscall::CAP_MMIO) {
        fail("SILVERD: HOWL CAP GRANT FAILED\r\n");
    }
    if !syscall::console_set(howl_cap) {
        fail("SILVERD: CONSOLE HANDOFF FAILED\r\n");
    }

    // 4. Howler (UART) reads the console and feeds Howl's mailbox.
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