//! secprobe: native EL0 regressions for the M2 capability gates.
//!
//! Run from the shell (which grants it `CAP_MMIO` for the run):
//!
//! ```text
//! secprobe grant_foreign    # F3: delegation only to self / own child
//! secprobe dma_cross_read   # F2: a foreign DMA window needs CAP_DMA_SHARE
//! ```
//!
//! `grant_foreign` spawns an idle child, then tries to delegate a scoped
//! right (CAP_WAIT) to (a) the shell, which is not a child of the probe,
//! (b) the probe itself and (c) the idle child. Only (a) must be refused.
//!
//! `dma_cross_read` builds a three-task tree: the orchestrator spawns an
//! owner (allocates a 4 KiB DMA run) and an attacker (has CAP_MMIO but no
//! capability naming the owner). The attacker's first map of the owner's
//! physical window must fail. The orchestrator then uses its spawn handle
//! over the owner to grant the attacker `CAP_DMA_SHARE` for that owner — the
//! same path silverd uses — after which the map must succeed, the attacker
//! writes a byte and the owner must observe it through its own mapping.
//!
//! The probe itself starts with no rights. It is not a way to mint a CAP_MMIO
//! task: a caller that cannot grant CAP_MMIO makes its own children fail the
//! handshake and the probe exits with a failure marker.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const ROLE_ORCHESTRATOR: u64 = 0;
const ROLE_GRANT_ONLY: u64 = 1;
const ROLE_DMA_ONLY: u64 = 2;
const ROLE_OWNER: u64 = 3;
const ROLE_ATTACKER: u64 = 4;
const ROLE_IDLE: u64 = 5;

const MSG_START: u64 = 1;
const MSG_OWNER_READY: u64 = 2;
const MSG_TEST: u64 = 3;
const MSG_TEST_RESULT: u64 = 4;
const MSG_RETRY: u64 = 5;
const MSG_RETRY_RESULT: u64 = 6;
const MSG_VERIFY: u64 = 7;
const MSG_VERIFY_RESULT: u64 = 8;
const MSG_RELEASE: u64 = 9;

const DMA_BYTES: u64 = 4096;
const MAGIC_BYTE: u8 = 0xA5;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(argument: u64) -> ! {
    match argument {
        ROLE_GRANT_ONLY => {
            let ok = run_grant_test();
            if ok {
                syscall::print("grant foreign ok\r\n");
            }
            syscall::exit_with_status(if ok { 0 } else { 1 });
        }
        ROLE_DMA_ONLY => {
            let ok = run_dma_test();
            if ok {
                syscall::print("dma cross read ok\r\n");
            }
            syscall::exit_with_status(if ok { 0 } else { 1 });
        }
        ROLE_OWNER => dma_owner(),
        ROLE_ATTACKER => dma_attacker(),
        ROLE_IDLE => loop {
            syscall::sleep(1000);
        },
        ROLE_ORCHESTRATOR => {
            let grant = run_grant_test();
            let dma = run_dma_test();
            if grant {
                syscall::print("grant foreign ok\r\n");
            }
            if dma {
                syscall::print("dma cross read ok\r\n");
            }
            syscall::exit_with_status(if grant && dma { 0 } else { 1 });
        }
        _ => syscall::exit_with_status(2),
    }
}

fn message(code: u64, first: u64, second: u64) -> ipc::Message {
    let mut message = ipc::Message::new(0, &[]);
    message.payload[0..8].copy_from_slice(&code.to_le_bytes());
    message.payload[8..16].copy_from_slice(&first.to_le_bytes());
    message.payload[16..24].copy_from_slice(&second.to_le_bytes());
    message
}

fn field(message: &ipc::Message, offset: usize) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&message.payload[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn send3(target: u64, code: u64, first: u64, second: u64) -> bool {
    syscall::send(target, &message(code, first, second))
}

fn receive() -> Option<ipc::Message> {
    let mut message = ipc::Message::new(0, &[]);
    if syscall::receive(&mut message) {
        Some(message)
    } else {
        None
    }
}

fn wait_for(code: u64) -> Option<ipc::Message> {
    for _ in 0..8 {
        let message = receive()?;
        if field(&message, 0) == code {
            return Some(message);
        }
    }
    None
}

/// F3: `SYS_CAP_GRANT` must refuse a target that is not the caller itself or
/// a task the caller spawned. The same handle must still work for both
/// allowed recipients, so a refusal cannot be an unrelated capability error.
fn run_grant_test() -> bool {
    let me = syscall::task_id();
    let parent = syscall::getppid();
    let Some(idle_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_IDLE) else {
        return false;
    };
    let Some(idle) = syscall::cap_get_task_id(idle_cap) else {
        let _ = syscall::kill(idle_cap);
        return false;
    };

    // The shell is the probe's parent: a live, non-child target. A correct
    // gate refuses; a missing gate would hand it CAP_WAIT over the idle task.
    let foreign_denied = parent != 0
        && parent != me
        && !syscall::cap_grant(idle_cap, parent, syscall::CAP_WAIT);
    let self_ok = syscall::cap_grant(idle_cap, me, syscall::CAP_WAIT);
    let child_ok = syscall::cap_grant(idle_cap, idle, syscall::CAP_WAIT);

    let _ = syscall::kill(idle_cap);
    let _ = syscall::waitpid(idle);

    if !foreign_denied {
        syscall::print("grant foreign: a non-child pid was accepted\r\n");
    }
    if !self_ok {
        syscall::print("grant foreign: self-delegation rejected\r\n");
    }
    if !child_ok {
        syscall::print("grant foreign: own-child delegation rejected\r\n");
    }
    foreign_denied && self_ok && child_ok
}

/// F2: mapping another task's DMA window needs `CAP_DMA_SHARE` for that
/// owner. CAP_MMIO alone (which the attacker has) must not be enough.
fn run_dma_test() -> bool {
    let Some(owner_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_OWNER) else {
        return false;
    };
    let Some(owner) = syscall::cap_get_task_id(owner_cap) else {
        let _ = syscall::kill(owner_cap);
        return false;
    };
    let Some(attacker_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_ATTACKER) else {
        let _ = syscall::kill(owner_cap);
        return false;
    };
    let Some(attacker) = syscall::cap_get_task_id(attacker_cap) else {
        let _ = syscall::kill(owner_cap);
        let _ = syscall::kill(attacker_cap);
        return false;
    };

    // Both children need CAP_MMIO (map_phys checks it before the share gate);
    // the probe holds it because the shell granted it. A caller that cannot
    // grant it reaches the failure return below instead of proceeding.
    let mmio = syscall::cap_grant(owner_cap, owner, syscall::CAP_MMIO)
        && syscall::cap_grant(attacker_cap, attacker, syscall::CAP_MMIO);
    let started = mmio
        && send3(owner, MSG_START, 0, 0)
        && send3(attacker, MSG_START, 0, 0);

    let ready = if started {
        wait_for(MSG_OWNER_READY)
    } else {
        None
    };
    let (paddr, size) = match ready {
        Some(message) => (field(&message, 8), field(&message, 16)),
        None => (0, 0),
    };

    // Negative half: the attacker has no capability naming the owner yet.
    let denied = paddr != 0
        && send3(attacker, MSG_TEST, paddr, size)
        && wait_for(MSG_TEST_RESULT).is_some_and(|result| field(&result, 8) == 1);

    // Positive half: hand the attacker the target-scoped right over the owner.
    // The probe's spawn handle covers CAP_ALL, so this is the same delegation
    // silverd performs for the net driver.
    let granted = denied
        && syscall::cap_grant(owner_cap, attacker, syscall::CAP_DMA_SHARE)
        && send3(attacker, MSG_RETRY, 0, 0);
    let retry = if granted {
        wait_for(MSG_RETRY_RESULT)
    } else {
        None
    };
    let (mapped, wrote) = match retry {
        Some(result) => (field(&result, 8) == 1, field(&result, 16) == 1),
        None => (false, false),
    };

    // The owner must see the byte the attacker wrote through the shared run:
    // mapping success alone would not prove it is the same physical memory.
    let observed = granted
        && send3(owner, MSG_VERIFY, 0, 0)
        && wait_for(MSG_VERIFY_RESULT).is_some_and(|result| field(&result, 8) == 1);

    let _ = send3(owner, MSG_RELEASE, 0, 0);
    let _ = syscall::kill(attacker_cap);
    // A failed handshake can leave the owner waiting for MSG_START; kill both
    // so a broken grant fails the probe instead of hanging the shell.
    let _ = syscall::kill(owner_cap);
    let _ = syscall::waitpid(attacker);
    let _ = syscall::waitpid(owner);

    if !mmio || !started {
        syscall::print("dma cross read: children have no CAP_MMIO\r\n");
    }
    if paddr == 0 {
        syscall::print("dma cross read: owner did not publish a window\r\n");
    }
    if !denied {
        syscall::print("dma cross read: foreign window was mapped without a share right\r\n");
    }
    if !granted || !mapped || !wrote {
        syscall::print("dma cross read: share grant did not open the window\r\n");
    }
    if !observed {
        syscall::print("dma cross read: owner did not see the shared write\r\n");
    }
    denied && granted && mapped && wrote && observed
}

/// Owns a 4 KiB DMA run and publishes its physical address. The byte at the
/// start stays zero until the attacker writes MAGIC_BYTE through the granted
/// mapping.
fn dma_owner() -> ! {
    let parent = syscall::getppid();
    let Some(_start) = wait_for(MSG_START) else {
        syscall::exit_with_status(1);
    };
    let Some((virtual_address, physical_address)) = syscall::alloc_dma(DMA_BYTES) else {
        syscall::exit_with_status(1);
    };
    unsafe {
        core::ptr::write_volatile(virtual_address as *mut u8, 0);
    }
    if !send3(parent, MSG_OWNER_READY, physical_address, DMA_BYTES) {
        syscall::exit_with_status(1);
    }
    loop {
        let Some(message) = receive() else {
            syscall::exit_with_status(1);
        };
        match field(&message, 0) {
            MSG_VERIFY => {
                let value = unsafe { core::ptr::read_volatile(virtual_address as *const u8) };
                if !send3(parent, MSG_VERIFY_RESULT, u64::from(value == MAGIC_BYTE), 0) {
                    syscall::exit_with_status(1);
                }
            }
            MSG_RELEASE => syscall::exit(),
            _ => {}
        }
    }
}

/// Maps the owner's window twice: before the share grant it must be refused,
/// after it the mapping must succeed and accept a write.
fn dma_attacker() -> ! {
    let parent = syscall::getppid();
    let Some(_start) = wait_for(MSG_START) else {
        syscall::exit_with_status(1);
    };
    let Some(test) = wait_for(MSG_TEST) else {
        syscall::exit_with_status(1);
    };
    let paddr = field(&test, 8);
    let size = field(&test, 16);

    let before = syscall::map_phys(paddr, size);
    if !send3(parent, MSG_TEST_RESULT, u64::from(before.is_none()), 0) {
        syscall::exit_with_status(1);
    }

    let Some(_retry) = wait_for(MSG_RETRY) else {
        syscall::exit_with_status(1);
    };
    let mapped = syscall::map_phys(paddr, size);
    let mut wrote = false;
    if let Some(virtual_address) = mapped {
        unsafe {
            core::ptr::write_volatile(virtual_address as *mut u8, MAGIC_BYTE);
        }
        wrote = true;
    }
    let _ = send3(parent, MSG_RETRY_RESULT, u64::from(mapped.is_some()), u64::from(wrote));
    loop {
        syscall::sleep(1000);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
