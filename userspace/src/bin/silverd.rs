#![no_std]
#![no_main]

//! Minimal service supervisor.
//!
//! Five fixed services, no heap: Scout, the network driver, the network
//! server, Howl and Howler. The supervisor keeps one task id and one capability
//! per service, notices a dead task, and restarts it with bounded exponential
//! backoff. Dependencies are explicit: a new network driver means a new network
//! server, and a new shell means a new input driver (both cache their peer id).

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SCOUT: usize = 0;
const NET_DRIVER: usize = 1;
const NET_SERVER: usize = 2;
const HOWL: usize = 3;
const HOWLER: usize = 4;
const SERVICE_COUNT: usize = 5;

const SVC_RESTART: u8 = 0xF1;
/// Status request from the shell; the reply carries one running flag and one
/// restart counter per service, in table order.
const SVC_STATUS: u8 = 0xF2;
const TICKS_PER_SECOND: u64 = 100;
const SUPERVISOR_POLL_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 30_000;
/// A service that survives this long is treated as healthy again, so a later
/// crash starts from a short retry instead of inheriting an old long backoff.
const STABLE_TICKS: u64 = 60 * TICKS_PER_SECOND;

const SCOUT_READY_TIMEOUT_TICKS: u64 = 200;
const SCOUT_POLL_TICKS: u64 = 20;

#[derive(Clone, Copy)]
struct Service {
    name: &'static str,
    program: u64,
    task: u64,
    cap: u64,
    restarts: u32,
    started_at: u64,
    retry_at: u64,
}

impl Service {
    const fn new(name: &'static str, program: u64) -> Self {
        Self {
            name,
            program,
            task: 0,
            cap: 0,
            restarts: 0,
            started_at: 0,
            retry_at: 0,
        }
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("SILVERD: BOOTING (CONSOLE)\r\n");

    let mut services = [
        Service::new("scout", syscall::PROGRAM_SCOUT),
        Service::new("net_driver", syscall::PROGRAM_NET_DRIVER),
        Service::new("net_server", syscall::PROGRAM_NET_SERVER),
        Service::new("howl", syscall::PROGRAM_HOWL),
        Service::new("howler", syscall::PROGRAM_HOWLER),
    ];

    // Start in dependency order. A failure is not fatal any more: the same
    // service stays dead in the table and the supervisor retries it.
    for index in 0..SERVICE_COUNT {
        try_start(index, &mut services);
    }

    loop {
        drain_requests(&mut services);
        supervise(&mut services);
        syscall::sleep(SUPERVISOR_POLL_MS);
    }
}

/// One supervision pass: notice dead services, stop their dependents, and
/// retry every dead service whose backoff has expired.
/// One supervision pass: notice dead services, stop their dependents, and
/// retry every dead service whose backoff has expired.
fn supervise(services: &mut [Service; SERVICE_COUNT]) {
    let now = syscall::uptime_ticks();
    let mut dead = [false; SERVICE_COUNT];

    for index in 0..SERVICE_COUNT {
        let service = &mut services[index];
        if service.task == 0 || syscall::task_alive(service.task) {
            continue;
        }
        // The task is gone. Reap the zombie before dropping the capability,
        // otherwise every crash leaves a task-table slot occupied forever and
        // the 64-task limit becomes a slow denial of service.
        reap(service.cap);
        service.task = 0;
        service.cap = 0;
        if now.saturating_sub(service.started_at) >= STABLE_TICKS {
            service.restarts = 0;
        }
        service.retry_at = now;
        dead[index] = true;
    }

    // Services cache a peer task id, and the network pair shares DMA. Never
    // restart one half alone: the old peer either points at a dead task or
    // loses a mapping while it is still running.
    for index in 0..SERVICE_COUNT {
        if !dead[index] {
            continue;
        }
        match index {
            NET_DRIVER | NET_SERVER => stop_pair(NET_DRIVER, NET_SERVER, services),
            HOWL => stop_pair(HOWLER, HOWL, services),
            _ => {}
        }
    }

    for index in 0..SERVICE_COUNT {
        if services[index].task == 0 && now >= services[index].retry_at {
            try_start(index, services);
        }
    }
}

fn try_start(index: usize, services: &mut [Service; SERVICE_COUNT]) -> bool {
    let now = syscall::uptime_ticks();
    if services[index].task != 0 {
        return true;
    }
    if now < services[index].retry_at {
        return false;
    }

    let started = match index {
        SCOUT => start_scout(),
        NET_DRIVER => start_net_driver(services[index].started_at == 0),
        NET_SERVER => {
            // No driver means no attach address. Leave the server dead; the
            // next pass retries after the driver has been restarted.
            if services[NET_DRIVER].task == 0 {
                None
            } else {
                start_net_server(services[NET_DRIVER].task)
            }
        }
        HOWL => start_howl(),
        HOWLER => {
            if services[HOWL].task == 0 || !syscall::driver_bound(syscall::PROGRAM_HOWLER) {
                None
            } else {
                start_howler(services[HOWL].task)
            }
        }
        _ => None,
    };

    match started {
        Some((task, cap)) => {
            services[index].task = task;
            services[index].cap = cap;
            services[index].started_at = now;
            services[index].retry_at = 0;
            true
        }
        None => {
            services[index].restarts = services[index].restarts.saturating_add(1);
            services[index].retry_at =
                now.saturating_add(backoff_ticks(services[index].restarts));
            false
        }
    }
}

/// Reaps a killed or already-dead child. `wait` blocks until the reaper
/// publishes the zombie, which is bounded and keeps the task table clean.
fn reap(cap: u64) {
    if cap != 0 {
        let _ = syscall::wait(cap);
    }
}

fn stop_one(index: usize, services: &mut [Service; SERVICE_COUNT]) {
    let cap = services[index].cap;
    let task = services[index].task;
    if cap != 0 {
        // Wait only after the kill succeeded or the task is already gone;
        // waiting on a live task that refused the kill would wedge the
        // supervisor, which is worse than leaving it for the retry pass.
        let killed = syscall::kill(cap);
        if killed || (task != 0 && !syscall::task_alive(task)) {
            let _ = syscall::wait(cap);
        }
    }
    services[index].task = 0;
    services[index].cap = 0;
    services[index].retry_at = 0;
}

/// Stops both ends of a dependency pair in the order that avoids a live task
/// losing a shared DMA mapping underneath it.
fn stop_pair(
    first: usize,
    second: usize,
    services: &mut [Service; SERVICE_COUNT],
) {
    stop_one(first, services);
    stop_one(second, services);
}

fn backoff_ticks(restarts: u32) -> u64 {
    let shift = restarts.saturating_sub(1).min(5);
    let milliseconds = (1_000_u64 << shift).min(MAX_BACKOFF_MS);
    (milliseconds + 9) / 10
}

/// Handles `svc restart <name>` from the shell. Invalid requests are ignored;
/// the shell validates the name too, and a supervisor must not trust a peer
/// blindly.
fn drain_requests(services: &mut [Service; SERVICE_COUNT]) {
    let mut message = ipc::Message::new(0, &[]);
    while syscall::receive_timeout(&mut message, 0).is_ok() {
        let text = message.text();
        if text == b"SFS ERROR" {
            // Kept for the boot phase, while silverd still owns the console.
            syscall::print("SILVERD GOT SFS ERROR!\r\n");
            continue;
        }
        if text.first() == Some(&SVC_STATUS) {
            reply_status(message.sender, services);
            continue;
        }
        if text.first() != Some(&SVC_RESTART) {
            continue;
        }
        let name = &text[1..];
        let Some(index) = services
            .iter()
            .position(|service| service.name.as_bytes() == name)
        else {
            continue;
        };
        restart(index, services);
    }
}

fn reply_status(requester: u64, services: &[Service; SERVICE_COUNT]) {
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = SVC_STATUS;
    for (index, service) in services.iter().enumerate() {
        payload[1 + index] = if service.task != 0 { 1 } else { 0 };
        payload[1 + SERVICE_COUNT + index] = service.restarts.min(255) as u8;
    }
    let _ = syscall::send(requester, &ipc::Message::new(0, &payload));
}

fn restart(index: usize, services: &mut [Service; SERVICE_COUNT]) {
    match index {
        // Kill the DMA owner before the server that maps its pages. Restarting
        // the pair is the only safe manual operation: a stale peer id or a
        // revoked shared mapping would fault the survivor.
        NET_DRIVER | NET_SERVER => stop_pair(NET_DRIVER, NET_SERVER, services),
        HOWL => stop_pair(HOWLER, HOWL, services),
        _ => stop_one(index, services),
    }
    // `stop_one` cleared `retry_at`; the immediately following supervise pass
    // starts the pair in table order, so dependencies are rebuilt without a
    // busy wait here.
}

fn spawn_and_grant(program: u64, argument: u64, rights: u32) -> Option<(u64, u64)> {
    let cap = syscall::spawn(program, argument)?;
    let Some(task) = syscall::cap_get_task_id(cap) else {
        let _ = syscall::kill(cap);
        return None;
    };
    if rights != 0 && !syscall::cap_grant(cap, task, rights) {
        let _ = syscall::kill(cap);
        return None;
    }
    Some((task, cap))
}

fn start_scout() -> Option<(u64, u64)> {
    let (task, cap) = spawn_and_grant(syscall::PROGRAM_SCOUT, 0, syscall::CAP_MMIO)?;
    // Readiness is a hint: a slow Scout must not kill the boot or block the
    // supervisor forever. Its clients retry their lookups.
    if !await_scout_ready(task) {
        syscall::print("SILVERD: SCOUT READY TIMEOUT, CONTINUING\r\n");
    }
    Some((task, cap))
}

fn start_net_driver(lend_console: bool) -> Option<(u64, u64)> {
    let (task, cap) = spawn_and_grant(
        syscall::PROGRAM_NET_DRIVER,
        0,
        syscall::CAP_PCI | syscall::CAP_MMIO,
    )?;
    // Only the first start lends the console: a restart must not steal the
    // shell's console for a second.
    if lend_console && syscall::console_set(cap) {
        syscall::sleep(1000);
        let _ = syscall::console_set(syscall::task_id());
    }
    Some((task, cap))
}

fn start_net_server(driver: u64) -> Option<(u64, u64)> {
    spawn_and_grant(syscall::PROGRAM_NET_SERVER, driver, syscall::CAP_MMIO)
}

fn start_howl() -> Option<(u64, u64)> {
    let (task, cap) = spawn_and_grant(syscall::PROGRAM_HOWL, 0, syscall::CAP_MMIO)?;
    if !syscall::console_set(cap) {
        let _ = syscall::kill(cap);
        return None;
    }
    Some((task, cap))
}

fn start_howler(howl: u64) -> Option<(u64, u64)> {
    spawn_and_grant(syscall::PROGRAM_HOWLER, howl, syscall::CAP_MMIO)
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
            && message.text() == b"SCOUT READY"
        {
            return true;
        }
        remaining = remaining.saturating_sub(SCOUT_POLL_TICKS);
    }
    false
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
