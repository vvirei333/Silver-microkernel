#![no_std]
#![no_main]

//! The interactive shell: entry point, input loop and the UART driver link.
//!
//! Commands and their implementations live in shell/; this file only reads
//! keystrokes, edits the line and hands complete commands to the dispatcher.

#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../shell/mod.rs"]
mod shell;

use core::panic::PanicInfo;

use shell::{echo, execute, fastfetch, find_task_id, find_task_id_preferring, prompt, TICKS_PER_SECOND};

/// One command line. 256 bytes are needed for `echo sig=<128 hex> >> <path>`;
/// the older 96-byte limit silently truncated signing commands.
const LINE_CAPACITY: usize = 256;

/// IPC opcode for a button press reported by `gpu_driver` (the shell owns the
/// console, so it prints `GUI: Button '<name>' pressed!`). Matches the value in
/// `gpu_driver.rs`.
const GUI_BUTTON_PRESSED: u8 = 2;

/// Re-resolves Howler once a second so a respawned driver is picked up and a
/// dead one is noticed without polling the task table on every keystroke.
const HOWLER_REFRESH_TICKS: u64 = TICKS_PER_SECOND;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(howler_id: u64) -> ! {
    syscall::print("\x1b[2J\x1b[H");
    fastfetch();
    syscall::register("howl");
    syscall::print("\r\nhowl ready. Type 'help' for commands.\r\n");
    prompt();

    let mut line = [0_u8; LINE_CAPACITY];
    let mut length = 0_usize;
    let mut howler = HowlerLink::new(howler_id);
    loop {
        let Some(byte) = read_input(&mut howler) else {
            // Only reached on a mailbox timeout or without a UART driver:
            // `read_input` blocks on the mailbox, so this is not the input poll
            // path and the tick spent here is immaterial.
            syscall::sleep(1);
            continue;
        };
        match byte {
            b'\r' | b'\n' => {
                syscall::print("\r\n");
                if let Ok(command) = core::str::from_utf8(&line[..length]) {
                    execute(command);
                }
                length = 0;
                prompt();
            }
            0x08 | 0x7F => {
                if length > 0 {
                    length -= 1;
                    syscall::print("\x08 \x08");
                }
            }
            0x20..=0x7E => {
                if length < LINE_CAPACITY {
                    line[length] = byte;
                    length += 1;
                    echo(byte);
                }
            }
            _ => {}
        }
    }
}

/// Tracks the UART driver task the shell reads keystrokes from.
///
/// silverd still passes the driver's task ID as the boot argument, but that
/// value is only a hint: the ID is re-resolved from the live task table
/// whenever the cached one stops being alive, so the shell never depends on the
/// spawn order chosen by `silverd` or by a manual `spawn howler`.
struct HowlerLink {
    id: u64,
    next_check: u64,
}

impl HowlerLink {
    fn new(id: u64) -> Self {
        Self { id, next_check: 0 }
    }

    /// Returns the live Howler task, refreshing the cached ID at most once per
    /// `HOWLER_REFRESH_TICKS`. The boot argument is verified against the task
    /// table first: it is only a hint, and accepting it blindly would make the
    /// shell poll the wrong mailbox forever.
    fn resolve(&mut self) -> Option<u64> {
        let now = syscall::uptime_ticks();
        if self.id != 0 {
            if now < self.next_check {
                return Some(self.id);
            }
            self.next_check = now.saturating_add(HOWLER_REFRESH_TICKS);
            if let Some(id) = find_task_id_preferring(syscall::PROGRAM_HOWLER, self.id) {
                self.id = id;
                return Some(id);
            }
            self.id = 0;
            return None;
        }

        if now < self.next_check {
            return None;
        }
        self.next_check = now.saturating_add(HOWLER_REFRESH_TICKS);
        self.id = find_task_id(syscall::PROGRAM_HOWLER).unwrap_or(0);
        if self.id == 0 {
            None
        } else {
            Some(self.id)
        }
    }
}

/// How long the shell parks on its mailbox before looking around again.
///
/// Input used to be polled: `receive_timeout(.., 0)` in a loop with
/// `sleep(1)` between attempts, i.e. a wakeup every tick and up to 10 ms of
/// typing latency. A blocking receive with a long timeout is woken by the
/// UART driver's first byte (near-zero latency) and costs nothing while idle;
/// the timeout only decides how often the shell re-resolves a restarted Howler.
const INPUT_WAIT_TICKS: u64 = 100;

fn read_input(howler: &mut HowlerLink) -> Option<u8> {
    let Some(howler_id) = howler.resolve() else {
        // No userspace UART driver (for example the first Redmi 10C boot,
        // where only Scout and Howl exist). Howl owns the console, so the
        // kernel console is a legal non-blocking input source.
        return syscall::read();
    };

    loop {
        let mut message = ipc::Message::new(0, &[]);
        // Blocking receive. Timeout or a vanished sender ends this attempt so
        // the caller can re-resolve Howler after a supervisor restart.
        if syscall::receive_timeout(&mut message, INPUT_WAIT_TICKS).is_err() {
            return None;
        }
        if message.sender == howler_id && message.payload[0] == 0 {
            return Some(message.payload[1]);
        }
        if message.payload[0] == GUI_BUTTON_PRESSED {
            report_gui_press(&message.payload[1..]);
        }
    }
}

/// Prints a button-press notification received from `gpu_driver`. This is what
/// makes a GUI click visible in the console: `gpu_driver` does not own the
/// console, so its own `syscall::print` would be discarded.
fn report_gui_press(label: &[u8]) {
    let end = label
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(label.len());
    let name = core::str::from_utf8(&label[..end]).unwrap_or("?");
    syscall::print("GUI: Button '");
    syscall::print(name);
    syscall::print("' pressed!\r\n");
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
