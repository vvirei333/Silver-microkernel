//! secprobe: native EL0 regressions for the capability and IPC gates.
//!
//! Run from the shell (which grants it `CAP_MMIO` for the run):
//!
//! ```text
//! secprobe grant_foreign      # F3: delegation only to self / own child
//! secprobe dma_cross_read     # F2: a foreign DMA range needs a share grant
//! secprobe dma_region_confine # F2: a grant for range A must not map range B
//! secprobe mailbox_quota      # F1: one sender cannot pin a mailbox
//! ```
//!
//! The probe itself starts with no rights. It is not a way to mint a CAP_MMIO
//! task: a caller that cannot grant CAP_MMIO makes the children fail the
//! handshake and the probe exits with a failure marker.
//!
//! DMA sharing is owner-consented and region-scoped: the task that owns the
//! run calls `SYS_DMA_GRANT_SELF(recipient, base, size)`. The attacker has
//! CAP_MMIO but no share capability until that call, and even then only the
//! named range is mappable.

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
const ROLE_MAILBOX_ONLY: u64 = 3;
const ROLE_REGION_ONLY: u64 = 4;
const ROLE_IRQ_ONLY: u64 = 5;
const ROLE_MMIO_ONLY: u64 = 6;

const ROLE_DMA_ATTACKER: u64 = 10;
const ROLE_MAIL_VICTIM: u64 = 11;
const ROLE_MAIL_ATTACKER: u64 = 12;
const ROLE_IDLE: u64 = 13;
const ROLE_IRQ_VICTIM: u64 = 20;

const MSG_START: u64 = 1;
const MSG_DMA_TEST: u64 = 2;
const MSG_DMA_RESULT: u64 = 3;
const MSG_DMA_RETRY: u64 = 4;
const MSG_DMA_RETRY_RESULT: u64 = 5;
const MSG_REGION_RESULT: u64 = 7;
const MSG_MAIL_START: u64 = 8;
const MSG_MAIL_RESULT: u64 = 9;
const MSG_MAIL_GO: u64 = 10;
const MSG_MAIL_ALIVE: u64 = 11;
const MSG_IRQ_BOUND: u64 = 12;
/// QEMU `virt` GIC distributor. The probe maps it NC and reads the SPI enable
/// bit; this is a hardware fact, not a kernel-provided diagnostic.
const GICD_BASE: u64 = 0x0800_0000;
const GICD_SIZE: u64 = 0x1_0000;
const GICD_ISENABLER: u64 = 0x100;
/// An unused, user-bindable SPI (32..=1019, not the timer, not the UART).
const PROBE_INTID: u64 = 64;
/// QEMU PL011: the range the shell does *not* delegate to the MMIO probe.
const UART_PHYS: u64 = 0x0900_0000;
const UART_SIZE: u64 = 0x1000;

const DMA_BYTES: u64 = 8192;
const REGION_BYTES: u64 = 4096;
const MAGIC_BYTE: u8 = 0xA5;
const REGION_MAGIC: u8 = 0x5A;
const MAIL_FILL: u64 = 64;
const MAIL_EXTRA: u64 = 10;
/// Sent by the shell after it grants CAP_MMIO. The probe waits for this
/// before touching DMA so a spawn on another CPU cannot race the grant and
/// exit before the capability arrives.
const MSG_SHELL_GO: u64 = 0x5343;

/// How long a parent waits for a child report before declaring the probe
/// failed. Long enough for TCG, short enough that a bug never hangs the shell.
const REPORT_TIMEOUT_TICKS: u64 = 400;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(argument: u64) -> ! {
    match argument {
        ROLE_GRANT_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_grant_test(), "grant foreign ok")
        }
        ROLE_DMA_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_dma_test(), "dma cross read ok")
        }
        ROLE_MAILBOX_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_mailbox_test(), "mailbox quota ok")
        }
        ROLE_REGION_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_region_test(), "dma region confine ok")
        }
        ROLE_IRQ_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_irq_test(), "irq orphan ok")
        }
        ROLE_MMIO_ONLY => {
            let _ = wait_for(MSG_SHELL_GO);
            finish(run_mmio_test(), "mmio range confine ok")
        }
        ROLE_DMA_ATTACKER => dma_attacker(),
        ROLE_MAIL_VICTIM => mail_victim(),
        ROLE_MAIL_ATTACKER => mail_attacker(),
        ROLE_IRQ_VICTIM => irq_victim(),
        ROLE_IDLE => loop {
            syscall::sleep(1000);
        },
        ROLE_ORCHESTRATOR => {
            let _ = wait_for(MSG_SHELL_GO);
            let grant = run_grant_test();
            let dma = run_dma_test();
            let mailbox = run_mailbox_test();
            let region = run_region_test();
            if grant {
                syscall::print("grant foreign ok\r\n");
            }
            if dma {
                syscall::print("dma cross read ok\r\n");
            }
            if mailbox {
                syscall::print("mailbox quota ok\r\n");
            }
            if region {
                syscall::print("dma region confine ok\r\n");
            }
            syscall::exit_with_status(if grant && dma && mailbox && region {
                0
            } else {
                1
            });
        }
        _ => syscall::exit_with_status(2),
    }
}

fn finish(ok: bool, marker: &str) -> ! {
    if ok {
        syscall::print(marker);
        syscall::print("\r\n");
    }
    syscall::exit_with_status(if ok { 0 } else { 1 });
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

fn message3(code: u64, first: u64, second: u64, third: u64) -> ipc::Message {
    let mut message = message(code, first, second);
    message.payload[24..32].copy_from_slice(&third.to_le_bytes());
    message
}

fn send_message(target: u64, message: &ipc::Message) -> bool {
    syscall::send(target, message)
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

/// Fail-fast variant: a child that never reports must not hang the shell.
fn wait_timeout(code: u64, ticks: u64) -> Option<ipc::Message> {
    let mut message = ipc::Message::new(0, &[]);
    for _ in 0..4 {
        match syscall::receive_timeout(&mut message, ticks) {
            Ok(()) => {
                if field(&message, 0) == code {
                    return Some(message);
                }
            }
            Err(_) => return None,
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

/// F2 negative + positive: the attacker has CAP_MMIO but no share right until
/// the *owner* grants the exact range. Mapping before the grant must fail;
/// after it must succeed and the byte must become visible in the owner's
/// mapping.
fn run_dma_test() -> bool {
    let Some((vaddr, paddr)) = syscall::alloc_dma(DMA_BYTES) else {
        syscall::print("dma cross read: owner allocation failed\r\n");
        return false;
    };
    unsafe {
        core::ptr::write_volatile(vaddr as *mut u8, 0);
    }
    let Some(attacker_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_DMA_ATTACKER) else {
        return false;
    };
    let Some(attacker) = syscall::cap_get_task_id(attacker_cap) else {
        let _ = syscall::kill(attacker_cap);
        return false;
    };
    let mmio = syscall::cap_grant(attacker_cap, attacker, syscall::CAP_MMIO);
    let started = mmio && send3(attacker, MSG_START, 0, 0);

    let denied = started
        && send_message(
            attacker,
            &message3(MSG_DMA_TEST, paddr, DMA_BYTES, 1),
        )
        && wait_timeout(MSG_DMA_RESULT, REPORT_TIMEOUT_TICKS)
            .is_some_and(|result| field(&result, 8) == 1);
    let granted = denied
        && syscall::dma_grant_self(attacker, paddr, DMA_BYTES)
        && send3(attacker, MSG_DMA_RETRY, 0, 0);
    let (mapped, wrote) = match if granted {
        wait_timeout(MSG_DMA_RETRY_RESULT, REPORT_TIMEOUT_TICKS)
    } else {
        None
    } {
        Some(result) => (field(&result, 8) == 1, field(&result, 16) == 1),
        None => (false, false),
    };
    let observed = mapped
        && wrote
        && unsafe { core::ptr::read_volatile(vaddr as *const u8) } == MAGIC_BYTE;

    let _ = syscall::kill(attacker_cap);
    let _ = syscall::waitpid(attacker);

    if !mmio || !started {
        syscall::print("dma cross read: attacker has no CAP_MMIO\r\n");
    }
    if !denied {
        syscall::print("dma cross read: foreign range was mapped without a share grant\r\n");
    }
    if !granted || !mapped || !wrote {
        syscall::print("dma cross read: owner grant did not open the range\r\n");
    }
    if !observed {
        syscall::print("dma cross read: owner did not see the shared write\r\n");
    }
    denied && granted && mapped && wrote && observed
}

/// F2 region confinement: a grant for the first 4 KiB must not authorize the
/// second 4 KiB of the same run.
fn run_region_test() -> bool {
    let Some((vaddr, paddr)) = syscall::alloc_dma(DMA_BYTES) else {
        syscall::print("dma region confine: owner allocation failed\r\n");
        return false;
    };
    unsafe {
        core::ptr::write_volatile(vaddr as *mut u8, 0);
    }
    let Some(attacker_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_DMA_ATTACKER) else {
        return false;
    };
    let Some(attacker) = syscall::cap_get_task_id(attacker_cap) else {
        let _ = syscall::kill(attacker_cap);
        return false;
    };
    let mmio = syscall::cap_grant(attacker_cap, attacker, syscall::CAP_MMIO);
    let started = mmio && send3(attacker, MSG_START, 0, 0);
    let granted =
        started && syscall::dma_grant_self(attacker, paddr, REGION_BYTES);
    let region_result = if granted {
        let _ = send_message(
            attacker,
            &message3(MSG_DMA_TEST, paddr, DMA_BYTES, 2),
        );
        wait_timeout(MSG_REGION_RESULT, REPORT_TIMEOUT_TICKS)
    } else {
        None
    };
    let (first_ok, second_denied) = match region_result {
        Some(result) => (field(&result, 8) == 1, field(&result, 16) == 1),
        None => (false, false),
    };
    let observed = first_ok
        && second_denied
        && unsafe { core::ptr::read_volatile(vaddr as *const u8) } == REGION_MAGIC;

    let _ = syscall::kill(attacker_cap);
    let _ = syscall::waitpid(attacker);

    if !mmio || !started || !granted {
        syscall::print("dma region confine: setup failed\r\n");
    }
    if !first_ok {
        syscall::print("dma region confine: granted range was not mapped\r\n");
    }
    if !second_denied {
        syscall::print("dma region confine: grant escaped its region\r\n");
    }
    if !observed {
        syscall::print("dma region confine: owner did not see the write\r\n");
    }
    first_ok && second_denied && observed
}

/// M3.1 range check: the shell delegates only the GIC range. Mapping the GIC
/// must succeed; mapping the UART (a real device range the probe does not own)
/// must fail even though the probe holds CAP_MMIO.
fn run_mmio_test() -> bool {
    let gic_ok = syscall::map_phys_nc(GICD_BASE, GICD_SIZE).is_some();
    let uart_ok = syscall::map_phys(UART_PHYS, UART_SIZE).is_some();
    if !gic_ok {
        syscall::print("mmio range confine: granted GIC range was refused\r\n");
    }
    if uart_ok {
        syscall::print("mmio range confine: unrelated UART range was mapped\r\n");
    }
    gic_ok && !uart_ok
}

/// R3: the reaper must disable a dead owner's bound INTID. The probe reads
/// the GIC distributor's ISENABLER bit (NC mapping) before and after the kill;
/// a leftover enable bit means the next hardware interrupt would take the
/// unknown-INTID path and charge the unmaskable streak.
fn run_irq_test() -> bool {
    let Some(gic) = syscall::map_phys_nc(GICD_BASE, GICD_SIZE) else {
        syscall::print("irq orphan: cannot map the GIC distributor\r\n");
        return false;
    };
    let clean_before = !spi_enabled(gic);
    let Some(victim_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_IRQ_VICTIM) else {
        return false;
    };
    let Some(victim) = syscall::cap_get_task_id(victim_cap) else {
        let _ = syscall::kill(victim_cap);
        return false;
    };
    // Binding needs CAP_IRQ; the shell granted it to this probe for the run.
    let granted = syscall::cap_grant(victim_cap, victim, syscall::CAP_IRQ)
        && send3(victim, MSG_START, 0, 0);
    let bound = granted
        && wait_timeout(MSG_IRQ_BOUND, REPORT_TIMEOUT_TICKS)
            .is_some_and(|message| field(&message, 8) == 1);
    let enabled_before = bound && spi_enabled(gic);

    let _ = syscall::kill(victim_cap);
    let _ = syscall::waitpid(victim);
    // waitpid returns only after the reaper has released the task.
    let enabled_after = spi_enabled(gic);
    let rebound = syscall::bind_irq(PROBE_INTID);

    if !clean_before {
        syscall::print("irq orphan: INTID was already enabled\r\n");
    }
    if !granted || !bound {
        syscall::print("irq orphan: victim could not bind the SPI\r\n");
    }
    if !enabled_before {
        syscall::print("irq orphan: bind did not enable the INTID\r\n");
    }
    if enabled_after {
        syscall::print("irq orphan: reaper left the dead task's INTID enabled\r\n");
    }
    if !rebound {
        syscall::print("irq orphan: INTID could not be rebound\r\n");
    }
    clean_before && bound && enabled_before && !enabled_after && rebound
}

fn spi_enabled(gic: u64) -> bool {
    let word = (PROBE_INTID / 32) as u64 * 4;
    let bit = 1_u32 << (PROBE_INTID % 32);
    let value = read32(gic + GICD_ISENABLER + word);
    value & bit != 0
}

fn read32(address: u64) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn irq_victim() -> ! {
    let parent = syscall::getppid();
    let Some(_start) = wait_for(MSG_START) else {
        syscall::exit_with_status(1);
    };
    let bound = syscall::bind_irq(PROBE_INTID);
    let _ = send3(parent, MSG_IRQ_BOUND, u64::from(bound), 0);
    loop {
        syscall::sleep(1000);
    }
}

/// F1: a single sender may leave at most `MAIL_FILL` unread messages in one
/// mailbox. Further *blocking* sends must return `QueueFull` immediately, not
/// pin the sender; the victim must still be able to drain and answer.
fn run_mailbox_test() -> bool {
    let Some(victim_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_MAIL_VICTIM) else {
        return false;
    };
    let Some(victim) = syscall::cap_get_task_id(victim_cap) else {
        let _ = syscall::kill(victim_cap);
        return false;
    };
    let Some(attacker_cap) = syscall::spawn(syscall::PROGRAM_SECPROBE, ROLE_MAIL_ATTACKER) else {
        let _ = syscall::kill(victim_cap);
        return false;
    };
    let Some(attacker) = syscall::cap_get_task_id(attacker_cap) else {
        let _ = syscall::kill(victim_cap);
        let _ = syscall::kill(attacker_cap);
        return false;
    };

    let started = send3(attacker, MSG_MAIL_START, victim, 0);
    let result = if started {
        wait_timeout(MSG_MAIL_RESULT, REPORT_TIMEOUT_TICKS)
    } else {
        None
    };
    let (filled, all_rejected) = match result {
        Some(message) => (field(&message, 8) == 1, field(&message, 16) == 1),
        None => (false, false),
    };

    let alive = filled
        && all_rejected
        && send3(victim, MSG_MAIL_GO, 0, 0)
        && wait_timeout(MSG_MAIL_ALIVE, REPORT_TIMEOUT_TICKS)
            .is_some_and(|message| field(&message, 8) >= MAIL_FILL);

    let _ = syscall::kill(attacker_cap);
    let _ = syscall::kill(victim_cap);
    let _ = syscall::waitpid(attacker);
    let _ = syscall::waitpid(victim);

    if !started {
        syscall::print("mailbox quota: attacker did not start\r\n");
    }
    if !filled {
        syscall::print("mailbox quota: could not queue the per-sender quota\r\n");
    }
    if !all_rejected {
        syscall::print("mailbox quota: blocking send was accepted past the quota\r\n");
    }
    if !alive {
        syscall::print("mailbox quota: victim could not drain and answer\r\n");
    }
    filled && all_rejected && alive
}

fn dma_attacker() -> ! {
    let parent = syscall::getppid();
    let Some(_start) = wait_for(MSG_START) else {
        syscall::exit_with_status(1);
    };
    let Some(test) = wait_timeout(MSG_DMA_TEST, REPORT_TIMEOUT_TICKS) else {
        syscall::exit_with_status(1);
    };
    match field(&test, 24) {
        1 => {
            let paddr = field(&test, 8);
            let size = field(&test, 16);
            let before = syscall::map_phys(paddr, size);
            if !send3(parent, MSG_DMA_RESULT, u64::from(before.is_none()), 0) {
                syscall::exit_with_status(1);
            }
            let Some(_retry) = wait_timeout(MSG_DMA_RETRY, REPORT_TIMEOUT_TICKS) else {
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
            let _ = send3(
                parent,
                MSG_DMA_RETRY_RESULT,
                u64::from(mapped.is_some()),
                u64::from(wrote),
            );
        }
        2 => {
            let paddr = field(&test, 8);
            let first = syscall::map_phys(paddr, REGION_BYTES);
            let mut first_ok = false;
            if let Some(virtual_address) = first {
                unsafe {
                    core::ptr::write_volatile(virtual_address as *mut u8, REGION_MAGIC);
                }
                first_ok = true;
            }
            let second = syscall::map_phys(paddr + REGION_BYTES, REGION_BYTES);
            let _ = send3(
                parent,
                MSG_REGION_RESULT,
                u64::from(first_ok),
                u64::from(second.is_none()),
            );
        }
        _ => {}
    }
    loop {
        syscall::sleep(1000);
    }
}

/// Sleeps first so the attacker can fill the quota without racing a drain,
/// then drains the queued fill messages until the go signal and reports how
/// many it saw.
fn mail_victim() -> ! {
    let parent = syscall::getppid();
    syscall::sleep(1000);
    let mut count = 0u64;
    loop {
        let Some(message) = receive() else {
            syscall::exit_with_status(1);
        };
        match field(&message, 0) {
            MSG_MAIL_START => count += 1,
            MSG_MAIL_GO => {
                let _ = send3(parent, MSG_MAIL_ALIVE, count, 0);
                loop {
                    syscall::sleep(1000);
                }
            }
            _ => {}
        }
    }
}

fn mail_attacker() -> ! {
    let parent = syscall::getppid();
    let Some(start) = wait_timeout(MSG_MAIL_START, REPORT_TIMEOUT_TICKS) else {
        syscall::exit_with_status(1);
    };
    let victim = field(&start, 8);

    let mut filled = 0u64;
    for index in 0..MAIL_FILL {
        if syscall::try_send(victim, &message(MSG_MAIL_START, index, 0)) {
            filled += 1;
        }
    }
    let mut rejected = 0u64;
    for index in 0..MAIL_EXTRA {
        // Must be QueueFull and immediate; a bug that blocks here is caught
        // by the parent's report timeout.
        if !syscall::send(victim, &message(MSG_MAIL_START, MAIL_FILL + index, 0)) {
            rejected += 1;
        }
    }
    let _ = send3(
        parent,
        MSG_MAIL_RESULT,
        u64::from(filled == MAIL_FILL),
        u64::from(rejected == MAIL_EXTRA),
    );
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
