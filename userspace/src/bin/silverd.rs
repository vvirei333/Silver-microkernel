#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const HOWL_TASK_ID: u64 = 3;
const HOWLER_TASK_ID: u64 = 4;
const BLOCK_TASK_ID: u64 = 5;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("SILVERD: BOOTING\r\n");

    let silverd_id = syscall::task_id();
    let (scout_cap, scout_id) = spawn(syscall::PROGRAM_SCOUT, silverd_id);
    let _ = scout_cap;
    wait_for_scout(scout_id);

    let (howl_cap, howl_id) = spawn(syscall::PROGRAM_HOWL, HOWLER_TASK_ID);
    if howl_id != HOWL_TASK_ID {
        fail("SILVERD: UNEXPECTED HOWL ID\r\n");
    }
    if !syscall::cap_grant(howl_cap, howl_id, syscall::CAP_MMIO) {
        fail("SILVERD: HOWL CAP GRANT FAILED\r\n");
    }
    if !syscall::console_set(howl_cap) {
        fail("SILVERD: CONSOLE HANDOFF FAILED\r\n");
    }

    let (howler_cap, howler_id) = spawn(syscall::PROGRAM_HOWLER, howl_id);
    if howler_id != HOWLER_TASK_ID {
        fail("SILVERD: UNEXPECTED HOWLER ID\r\n");
    }
    if !syscall::cap_grant(howler_cap, howler_id, syscall::CAP_MMIO | syscall::CAP_IRQ) {
        fail("SILVERD: HOWLER CAP GRANT FAILED\r\n");
    }

    let (block_cap, block_id) = spawn(syscall::PROGRAM_BLOCK_DRIVER, 0);
    if block_id != BLOCK_TASK_ID {
        fail("SILVERD: UNEXPECTED BLOCK DRIVER ID\r\n");
    }
    if !syscall::cap_grant(block_cap, block_id, syscall::CAP_PCI | syscall::CAP_MMIO) {
        fail("SILVERD: BLOCK DRIVER CAP GRANT FAILED\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
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

fn wait_for_scout(scout_id: u64) {
    loop {
        if !syscall::task_alive(scout_id) {
            fail("SILVERD: SCOUT EXITED\r\n");
        }
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive(&mut message)
            && message.sender == scout_id
            && message.text() == b"SCOUT READY"
        {
            return;
        }
    }
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
