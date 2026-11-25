//! Process commands: task table views, spawn/kill/wait, and program loading.
//!
//! The SFS is the source of programs; this module owns the loader paths and
//! the child-reaping bookkeeping.

use super::*;
use crate::{ipc, syscall};

const TASK_LIST_CAPACITY: usize = 32;

/// QEMU virt GIC distributor: the shell holds this range only to delegate it
/// to the IRQ/MMIO probes.
const GICD_BASE: u64 = 0x0800_0000;

const GICD_SIZE: u64 = 0x1_0000;

const EMPTY_TASK: syscall::TaskInfo = syscall::TaskInfo {
    id: 0,
    state: 0,
    program: 0,
    argument: 0,
};

const TASK_DEAD: u64 = 6;

const TASK_STOPPED: u64 = 7;

/// Resolves the live task whose executable is `program`.
///
/// Task IDs depend on the spawn order of the whole boot, so nothing may assume
/// them. Dead and stopped tasks are skipped: a respawned driver must be found by
/// its program, not by a stale ID that still sits in the task table.
pub fn find_task_id(program: u64) -> Option<u64> {
    find_task_id_preferring(program, 0)
}

/// Same as [`find_task_id`], but uses `preferred` when that task is alive and
/// really runs `program`. A hint is never trusted on liveness alone.
///
/// The most recently spawned match wins. That matters for a hardware driver: a
/// respawned instance re-runs device setup and therefore owns the device, while
/// the older instance is left talking to a virtqueue the device no longer uses.
pub fn find_task_id_preferring(program: u64, preferred: u64) -> Option<u64> {
    let mut tasks = [EMPTY_TASK; TASK_LIST_CAPACITY];
    let count = syscall::task_list(&mut tasks)?;
    let tasks = &tasks[..count.min(TASK_LIST_CAPACITY)];
    let live = |task: &syscall::TaskInfo| {
        task.program == program && task.state != TASK_DEAD && task.state != TASK_STOPPED
    };
    tasks
        .iter()
        .rev()
        .find(|task| task.id == preferred && live(task))
        .or_else(|| tasks.iter().rev().find(|task| live(task)))
        .map(|task| task.id)
}

/// Releases the driver's shared DMA window. Best effort: a driver that already
/// exited is not an error, its buffer is reclaimed with the task.
pub(super) fn acknowledge_disk_buffer(driver: u64) {
    let acknowledgement = ipc::Message::new(0, &[0xA5]);
    let _ = syscall::send(driver, &acknowledgement);
}

/// Waits for a reply from `task_id` only, bounded so a driver that dies without
/// answering cannot block the shell forever. Unrelated queued messages are
/// discarded, which is fine: nothing else sends to the interactive shell.
pub(super) fn receive_from(task_id: u64, buffer: &mut ipc::Message) -> bool {
    let deadline = syscall::uptime_ticks().saturating_add(RESPONSE_TIMEOUT_TICKS);
    loop {
        if !syscall::task_alive(task_id) {
            return false;
        }
        match syscall::receive_timeout(buffer, RESPONSE_POLL_TICKS) {
            Ok(()) => {
                if buffer.sender == task_id {
                    return true;
                }
            }
            Err(_) => {
                if syscall::uptime_ticks() >= deadline {
                    return false;
                }
            }
        }
    }
}

pub(super) fn run_utility(program: u64, arguments: &str) {
    // One atomic spawn+handoff: the child owns the console before its first
    // timeslice, so a two-line command cannot finish before the handoff (which
    // used to print "Could not assign the foreground console." and lose output).
    let Some(cap_id) = syscall::spawn_with_args_console(program, 0, arguments) else {
        syscall::print("Could not start program.\r\n");
        return;
    };
    let event = syscall::wait_event(cap_id);
    match event {
        Some(syscall::WaitEvent::Exited(0)) => {}
        Some(syscall::WaitEvent::Exited(status)) => {
            syscall::print("Program exited with status ");
            syscall::print_dec_u64(status);
            syscall::print("\r\n");
        }
        Some(syscall::WaitEvent::Stopped) => {
            syscall::print("Program CapId=");
            syscall::print_dec_u64(cap_id);
            syscall::print(" stopped\r\n");
        }
        None => syscall::print("Could not wait for program.\r\n"),
    }
}

/// Loads an ELF from the filesystem (`run <path>`) and runs it in the
/// foreground: the kernel reads the file, loads it and spawns it, then the
/// shell lends it the console and waits — mirroring `run_utility`, but with the
/// executable resolved from the VFS instead of the embedded program table.
pub(super) fn run_elf(path: &str) {
    run_elf_with(path, syscall::spawn_path, true);
}

/// `linuxstart <app>`: resolves `<app>-serve` first, then `<app>`, under
/// `/sfs` and `/sfs/apps`. `linuxstart phosh` therefore finds
/// `/sfs/phosh-serve` without the user remembering the suffix.
#[cfg(feature = "linux-abi")]
pub(super) fn run_linux_start(name: &str) {
    if name.is_empty() {
        syscall::print("Usage: linuxstart <app>  (e.g. linuxstart phosh)\r\n");
        return;
    }
    let mut path = [0_u8; 160];
    for suffix in [b"-serve".as_slice(), b"".as_slice()] {
        for prefix in [b"/sfs/".as_slice(), b"/sfs/apps/".as_slice()] {
            let total = prefix.len() + name.len() + suffix.len();
            if total >= path.len() {
                continue;
            }
            path[..prefix.len()].copy_from_slice(prefix);
            path[prefix.len()..prefix.len() + name.len()].copy_from_slice(name.as_bytes());
            path[prefix.len() + name.len()..total].copy_from_slice(suffix);
            if let Ok(text) = core::str::from_utf8(&path[..total]) {
                if let Some(cap_id) = syscall::spawn_linux_path(text) {
                    run_spawned(cap_id, false);
                    return;
                }
            }
        }
    }
    syscall::print("linuxstart: no such app '");
    syscall::print(name);
    syscall::print("'\r\n");
}

/// Loads a foreign ELF (`linuxrun <path>`) and runs it with the Linux syscall
/// personality. The kernel assigns the console inside the spawn, before the
/// child can run on any CPU, so no `console_set` round trip is needed.
#[cfg(feature = "linux-abi")]
pub(super) fn run_linux_elf(path: &str) {
    run_elf_with(path, syscall::spawn_linux_path, false);
}

/// `linuxrun-<app>`: resolves `/sfs/apps/<app>` then `/sfs/<app>`, so a
/// installed bundle and a plain test binary share one short command.
#[cfg(feature = "linux-abi")]
pub(super) fn run_linux_alias(name: &str) {
    run_elf_with(name, spawn_linux_alias, false);
}

#[cfg(feature = "linux-abi")]
pub(super) fn spawn_linux_alias(name: &str) -> Option<u64> {
    if name.is_empty() {
        return None;
    }
    let mut path = [0_u8; 160];
    for prefix in [b"/sfs/apps/".as_slice(), b"/sfs/".as_slice()] {
        let total = prefix.len() + name.len();
        if total >= path.len() {
            continue;
        }
        path[..prefix.len()].copy_from_slice(prefix);
        path[prefix.len()..total].copy_from_slice(name.as_bytes());
        if let Ok(text) = core::str::from_utf8(&path[..total]) {
            if let Some(id) = syscall::spawn_linux_path(text) {
                return Some(id);
            }
        }
    }
    None
}

pub(super) fn run_elf_with(path: &str, spawn: fn(&str) -> Option<u64>, assign_console: bool) {
    if path.is_empty() {
        syscall::print("Usage: run <path>\r\n");
        return;
    }
    let Some(cap_id) = spawn(path) else {
        syscall::print("run: cannot load '");
        syscall::print(path);
        syscall::print("'\r\n");
        return;
    };
    run_spawned(cap_id, assign_console);
}

pub(super) fn run_spawned(cap_id: u64, assign_console: bool) {
    if assign_console && !syscall::console_set(cap_id) {
        syscall::print("run: could not assign the foreground console.\r\n");
        let _ = syscall::kill(cap_id);
        let _ = syscall::wait(cap_id);
        return;
    }
    let event = syscall::wait_event(cap_id);
    match event {
        Some(syscall::WaitEvent::Exited(0)) => {}
        Some(syscall::WaitEvent::Exited(status)) => {
            syscall::print("Program exited with status ");
            syscall::print_dec_u64(status);
            syscall::print("\r\n");
        }
        Some(syscall::WaitEvent::Stopped) => {
            syscall::print("Program CapId=");
            syscall::print_dec_u64(cap_id);
            syscall::print(" stopped\r\n");
        }
        None => syscall::print("run: could not wait for program.\r\n"),
    }
}

pub(super) fn foreground_task(cap_id: Option<&str>) {
    let Some(cap_id) = cap_id.and_then(parse_number) else {
        syscall::print("Usage: fg <cap>\r\n");
        return;
    };
    if !syscall::resume(cap_id) || !syscall::console_set(cap_id) {
        syscall::print("fg: invalid or exited task\r\n");
        return;
    }
    let event = syscall::wait_event(cap_id);
    match event {
        Some(syscall::WaitEvent::Exited(status)) => {
            syscall::print("Foreground CapId=");
            syscall::print_dec_u64(cap_id);
            syscall::print(" exited with status ");
            syscall::print_dec_u64(status);
            syscall::print("\r\n");
        }
        Some(syscall::WaitEvent::Stopped) => {
            syscall::print("Stopped CapId=");
            syscall::print_dec_u64(cap_id);
            syscall::print("\r\n");
        }
        None => syscall::print("fg: could not wait for task\r\n"),
    }
}

pub(super) fn kill_task(cap_id: Option<&str>) {
    let Some(cap_id) = cap_id.and_then(parse_number) else {
        syscall::print("Usage: kill <cap>\r\n");
        return;
    };
    if syscall::kill(cap_id) {
        syscall::print("Killed CapId=");
        syscall::print_dec_u64(cap_id);
        syscall::print("\r\n");
    } else {
        syscall::print("kill: invalid capability or task already exited\r\n");
    }
}

/// `svc restart <name>`: asks silverd to restart one supervised service.
/// The supervisor owns the child capabilities, so the shell cannot kill and
/// respawn a service without bypassing the restart policy.
pub(super) fn svc_command(action: Option<&str>, name: Option<&str>) {
    if action.is_none() {
        svc_status();
        return;
    }
    if action != Some("restart") {
        syscall::print("usage: svc restart <scout|net_driver|net_server|howl|howler>\r\n");
        return;
    }
    let Some(name) = name else {
        syscall::print("usage: svc restart <scout|net_driver|net_server|howl|howler>\r\n");
        return;
    };
    if !matches!(
        name,
        "scout" | "net_driver" | "net_server" | "howl" | "howler"
    ) {
        syscall::print("svc: unknown service '");
        syscall::print(name);
        syscall::print("'\r\n");
        return;
    }
    let Some(supervisor) = find_task_id(syscall::PROGRAM_SILVERD) else {
        syscall::print("svc: silverd is not running\r\n");
        return;
    };

    const SVC_RESTART: u8 = 0xF1;
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = SVC_RESTART;
    let bytes = name.as_bytes();
    let length = bytes.len().min(payload.len() - 1);
    payload[1..1 + length].copy_from_slice(&bytes[..length]);
    if syscall::send(supervisor, &ipc::Message::new(0, &payload)) {
        syscall::print("svc: restart '");
        syscall::print(name);
        syscall::print("' requested; run 'ps' in a second\r\n");
    } else {
        syscall::print("svc: supervisor mailbox is unavailable\r\n");
    }
}

/// `svc`: prints the supervisor's own view of its services. The supervisor is
/// the authority: `ps` shows scheduler states, this shows supervised liveness
/// and how often each service was restarted.
fn svc_status() {
    let Some(supervisor) = find_task_id(syscall::PROGRAM_SILVERD) else {
        syscall::print("svc: silverd is not running\r\n");
        return;
    };
    const SVC_STATUS: u8 = 0xF2;
    let mut request = [0_u8; ipc::PAYLOAD_SIZE];
    request[0] = SVC_STATUS;
    if !syscall::send(supervisor, &ipc::Message::new(0, &request)) {
        syscall::print("svc: supervisor mailbox is unavailable\r\n");
        return;
    }
    // Same bounded-wait pattern as the other driver replies: discard unrelated
    // traffic, give up when the deadline expires.
    let deadline = syscall::uptime_ticks().saturating_add(RESPONSE_TIMEOUT_TICKS);
    let mut reply = ipc::Message::new(0, &[]);
    let payload = loop {
        if !syscall::task_alive(supervisor) {
            syscall::print("svc: supervisor exited\r\n");
            return;
        }
        match syscall::receive_timeout(&mut reply, RESPONSE_POLL_TICKS) {
            Ok(()) if reply.sender == supervisor && reply.payload[0] == SVC_STATUS => {
                break reply.payload
            }
            Err(_) if syscall::uptime_ticks() >= deadline => {
                syscall::print("svc: no supervisor reply\r\n");
                return;
            }
            _ => {}
        }
    };
    const NAMES: [&str; 5] = ["scout", "net_driver", "net_server", "howl", "howler"];
    for (index, name) in NAMES.iter().enumerate() {
        let running = payload[1 + index] != 0;
        let restarts = payload[1 + 5 + index];
        syscall::print(name);
        for _ in 0..(11usize.saturating_sub(name.len())) {
            syscall::print(" ");
        }
        if running {
            syscall::print("running  restarts ");
        } else {
            syscall::print("down     restarts ");
        }
        syscall::print_dec_u64(u64::from(restarts));
        syscall::print("\r\n");
    }
}

pub(super) fn wait_task(cap_id: Option<&str>) {
    let Some(cap_id) = cap_id.and_then(parse_number) else {
        syscall::print("Usage: wait <cap>\r\n");
        return;
    };
    if let Some(status) = syscall::wait(cap_id) {
        syscall::print("CapId=");
        syscall::print_dec_u64(cap_id);
        syscall::print(" exited with status ");
        syscall::print_dec_u64(status);
        syscall::print("\r\n");
    } else {
        syscall::print("wait: unknown capability or status already consumed\r\n");
    }
}

pub(super) fn show_health() {
    let Some(info) = syscall::health_info() else {
        syscall::print("health: unavailable\r\n");
        return;
    };
    let used_pages = info.total_pages.saturating_sub(info.free_pages);
    let used_kib = used_pages.saturating_mul(4);
    syscall::print("WerewolfOS health v");
    syscall::print_dec_u64(info.version);
    syscall::print("\r\n");
    syscall::print("  frames: used ");
    syscall::print_dec_u64(used_pages);
    syscall::print(" / ");
    syscall::print_dec_u64(info.total_pages);
    syscall::print(" (");
    syscall::print_dec_u64(used_kib);
    syscall::print(" KiB)\r\n");
    syscall::print("  tasks:  live ");
    syscall::print_dec_u64(info.live_tasks);
    syscall::print(" zombie ");
    syscall::print_dec_u64(info.zombies);
    syscall::print("\r\n");
    syscall::print("  files:  open ");
    syscall::print_dec_u64(info.open_files);
    syscall::print("\r\n");
    syscall::print("  dma:    leaked ");
    syscall::print_dec_u64(info.dma_leaked_pages);
    syscall::print(" quarantined ");
    syscall::print_dec_u64(info.dma_quarantined_pages);
    syscall::print("\r\n  uptime: ");
    syscall::print_dec_u64(info.uptime_ticks);
    syscall::print(" ticks\r\n");
}

pub(super) fn show_tasks() {
    let mut tasks = [EMPTY_TASK; TASK_LIST_CAPACITY];
    let Some(count) = syscall::task_list(&mut tasks) else {
        syscall::print("Could not read the task list.\r\n");
        return;
    };

    syscall::print("ID  PROGRAM      ARG  STATE\r\n");
    for task in &tasks[..count] {
        syscall::print_dec_u64(task.id);
        syscall::print("   ");
        syscall::print(program_name(task.program));
        syscall::print("  ");
        syscall::print_dec_u64(task.argument);
        syscall::print("    ");
        syscall::print(state_name(task.state));
        syscall::print("\r\n");
    }
}

pub(super) fn program_name(program: u64) -> &'static str {
    match program {
        syscall::PROGRAM_DEMO => "demo        ",
        syscall::PROGRAM_SILVERD => "silverd     ",
        syscall::PROGRAM_SCOUT => "scout       ",
        syscall::PROGRAM_HOWL => "howl        ",
        syscall::PROGRAM_HOWLER => "howler      ",
        syscall::PROGRAM_LS => "ls          ",
        syscall::PROGRAM_CAT => "cat         ",
        syscall::PROGRAM_ECHO => "echo        ",
        syscall::PROGRAM_SAY => "say         ",
        syscall::PROGRAM_SL => "sl          ",
        syscall::PROGRAM_UTILS => "utils       ",
        syscall::PROGRAM_NET_DRIVER => "net_driver  ",
        syscall::PROGRAM_NET_SERVER => "net_server  ",
        syscall::PROGRAM_DYNAMIC => "elf         ",
        _ => "kernel      ",
    }
}

pub(super) fn state_name(state: u64) -> &'static str {
    match state {
        1 => "ready",
        2 => "running",
        3 => "sleeping",
        4 => "blocked",
        5 => "blocked-timeout",
        6 => "dead",
        7 => "stopped",
        _ => "unknown",
    }
}

/// Hands a hand-spawned driver the rights the kernel does not grant by itself.
///
/// `CapTable::grant` requires the caller (the shell) to hold the right it passes
/// on, so only `CAP_MMIO` is delegated here — that is the entry `silvershell`
/// receives. `CAP_PCI` is granted by the kernel at spawn time for the block
/// driver and is deliberately not held by the shell.
///
/// Spawning still succeeds when the grant fails; the task simply runs without
/// device access, which is a diagnosable state rather than a silent one.
pub(super) fn grant_device_rights(cap_id: u64, task_id: u64, program: u64) {
    let rights = match program {
        syscall::PROGRAM_NET_DRIVER
        | syscall::PROGRAM_NET_SERVER => syscall::CAP_MMIO,
        _ => return,
    };
    // Recipient must be the task ID: the CapId only names the shell's own entry
    // for that task, and the kernel resolves recipients by task ID.
    if !syscall::cap_grant(cap_id, task_id, rights) {
        syscall::print("Spawned, but CAP_MMIO grant failed: the task has no device access.\r\n");
    }
}

/// `secprobe <grant_foreign|dma_cross_read>`: native M2 capability
/// regressions. The probe binary has no initial rights, so a caller that
/// cannot delegate CAP_MMIO cannot turn it into a mapping proxy. The shell
/// is the trusted control plane and holds CAP_MMIO itself; it grants the
/// right to its own child for the run and gives it the foreground console.
pub(super) fn secprobe_command(test: Option<&str>) {
    let mode = match test {
        None => 0,
        Some("grant_foreign") => 1,
        Some("dma_cross_read") => 2,
        Some("mailbox_quota") => 3,
        Some("dma_region_confine") => 4,
        Some("irq_orphan") => 5,
        Some("mmio_range_confine") => 6,
        Some(_) => {
            syscall::print(
                "usage: secprobe <grant_foreign|dma_cross_read|mailbox_quota|dma_region_confine|irq_orphan|mmio_range_confine>\r\n",
            );
            return;
        }
    };
    let Some(cap_id) = syscall::spawn(syscall::PROGRAM_SECPROBE, mode) else {
        syscall::print("secprobe: cannot start the probe\r\n");
        return;
    };
    let Some(task_id) = syscall::cap_get_task_id(cap_id) else {
        let _ = syscall::kill(cap_id);
        syscall::print("secprobe: cannot resolve the probe task\r\n");
        return;
    };
    // Only the IRQ probe needs CAP_IRQ; everything else gets just CAP_MMIO.
    let rights = if mode == 5 {
        syscall::CAP_MMIO | syscall::CAP_IRQ
    } else {
        syscall::CAP_MMIO
    };
    if !syscall::cap_grant(cap_id, task_id, rights) {
        let _ = syscall::kill(cap_id);
        let _ = syscall::wait(cap_id);
        syscall::print("secprobe: capability grant failed\r\n");
        return;
    }
    // Device MMIO is region-scoped now. The IRQ and range probes map the GIC
    // distributor, so delegate exactly that range (the shell owns it as a
    // trusted control-plane capability; this is not ambient CAP_MMIO).
    if (mode == 5 || mode == 6)
        && !syscall::mmio_grant(task_id, GICD_BASE, GICD_SIZE)
    {
        let _ = syscall::kill(cap_id);
        let _ = syscall::wait(cap_id);
        syscall::print("secprobe: GIC range grant failed\r\n");
        return;
    }
    if !syscall::console_set(cap_id) {
        let _ = syscall::kill(cap_id);
        let _ = syscall::wait(cap_id);
        syscall::print("secprobe: cannot assign the console\r\n");
        return;
    }
    // Release the probe only after both the capability and the console are in
    // place: the child may have been scheduled on another CPU immediately
    // after spawn, and allocating DMA before the grant would fail the run.
    if !syscall::send(
        task_id,
        &ipc::Message::new(0, &0x5343_u64.to_le_bytes()),
    ) {
        let _ = syscall::kill(cap_id);
        let _ = syscall::wait(cap_id);
        syscall::print("secprobe: could not release the probe\r\n");
        return;
    }
    match syscall::wait_event(cap_id) {
        Some(syscall::WaitEvent::Exited(0)) => {}
        Some(syscall::WaitEvent::Exited(status)) => {
            syscall::print("secprobe: exit status ");
            syscall::print_dec_u64(status);
            syscall::print("\r\n");
        }
        Some(syscall::WaitEvent::Stopped) => syscall::print("secprobe: stopped\r\n"),
        None => syscall::print("secprobe: wait failed\r\n"),
    }
}

// ---------------------------------------------------------------------------
// Reaping fire-and-forget spawns
// ---------------------------------------------------------------------------

/// Cap handles of `spawn`/`bg` children whose exit nobody waits for.
///
/// The kernel keeps an exited task as a zombie record until its parent reaps
/// it (capped at 64, oldest statuses dropped), so eight `spawn` commands used to
/// leave `health` reporting `zombie 8` and `ps` listing dead tasks. The shell
/// now remembers the handles it did not wait for and reaps them here, without
/// ever blocking: a task that is still alive is skipped.
///
/// `static mut` matches the rest of this single-threaded program (see
/// `NET_WINDOW`); the shell runs on one task and nothing here is re-entered.
const UNREAPED_CAPACITY: usize = 16;

static mut UNREAPED: [(u64, u64); UNREAPED_CAPACITY] = [(0, 0); UNREAPED_CAPACITY];

static mut UNREAPED_NEXT: usize = 0;

pub(super) fn remember_unreaped(cap_id: u64) {
    let Some(task_id) = syscall::cap_get_task_id(cap_id) else {
        return;
    };
    unsafe {
        UNREAPED[UNREAPED_NEXT % UNREAPED_CAPACITY] = (cap_id, task_id);
        UNREAPED_NEXT = UNREAPED_NEXT.wrapping_add(1);
    }
}

pub(super) fn reap_unreaped() {
    unsafe {
        for slot in UNREAPED.iter_mut() {
            if slot.1 == 0 || syscall::task_alive(slot.1) {
                continue;
            }
            let _ = syscall::wait(slot.0);
            *slot = (0, 0);
        }
    }
}

/// `spawn <program> [arg|text]` / `bg <program> [arg|text]`.
///
/// `spawn` hands the console to the child and returns without waiting (the
/// console returns to the shell when the child exits); `bg` keeps it. That is
/// the difference the help text always promised but the old code did not
/// implement: both verbs ran the identical fire-and-forget spawn.
///
/// The argument used to have to be a decimal integer, so `spawn ls /sfs` and
/// `spawn cat /sfs/readme.txt` were rejected with "Argument must be a
/// non-negative decimal integer." A decimal argument still selects the numeric
/// form (programs like `howler` take an INTID); anything else is passed to the
/// child as its argument string.
pub(super) fn spawn_program(program: Option<&str>, argument: Option<&str>, background: bool) {
    let Some(program) = program else {
        syscall::print(
            "Usage: spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl|utils|wolfser|net_driver|net_server> [arg]\r\n",
        );
        return;
    };
    let program = match program {
        "demo" => syscall::PROGRAM_DEMO,
        "silverd" => syscall::PROGRAM_SILVERD,
        "scout" => syscall::PROGRAM_SCOUT,
        "howl" => syscall::PROGRAM_HOWL,
        "howler" => syscall::PROGRAM_HOWLER,
        "ls" => syscall::PROGRAM_LS,
        "cat" => syscall::PROGRAM_CAT,
        "echo" => syscall::PROGRAM_ECHO,
        "say" => syscall::PROGRAM_SAY,
        "sl" => syscall::PROGRAM_SL,
        "utils" => syscall::PROGRAM_UTILS,
        "wolfser" | "browser" => syscall::PROGRAM_WOLFSER,
        "net_driver" => syscall::PROGRAM_NET_DRIVER,
        "net_server" => syscall::PROGRAM_NET_SERVER,
        _ => {
            syscall::print("Unknown program. See 'help' for the program list.\r\n");
            return;
        }
    };
    let numeric = argument.and_then(parse_number);
    let text_argument = argument.filter(|_| numeric.is_none());
    let numeric = numeric.unwrap_or(0);

    let cap_id = match (background, text_argument) {
        (true, Some(text)) => syscall::spawn_with_args(program, numeric, text),
        (true, None) => syscall::spawn(program, numeric),
        (false, Some(text)) => syscall::spawn_with_args_console(program, numeric, text),
        (false, None) => syscall::spawn_with_args_console(program, numeric, ""),
    };
    if let Some(cap_id) = cap_id {
        if let Some(id) = syscall::cap_get_task_id(cap_id) {
            grant_device_rights(cap_id, id, program);
            remember_unreaped(cap_id);
            let mut line = syscall::Line::new();
            line.str("Spawned task ID=").dec(id);
            line.str(" CapId=").dec(cap_id).str("\r\n");
            line.emit();
        } else {
            syscall::print("Spawned, but could not resolve CapId.\r\n");
        }
    } else {
        syscall::print("Spawn failed.\r\n");
    }
}

pub(super) fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0_u64;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}
