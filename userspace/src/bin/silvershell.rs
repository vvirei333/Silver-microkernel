#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const LINE_CAPACITY: usize = 96;
const TICKS_PER_SECOND: u64 = 100;
const MIB: u64 = 1024 * 1024;

const WOLF: [&str; 8] = [
    "   /\\_/\\              ",
    "  ( o.o )             ",
    "   > ^ <              ",
    "  /|   |\\             ",
    " / |   | \\            ",
    "   |___|              ",
    "  /     \\             ",
    "                      ",
];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("\x1b[2J\x1b[H");
    fastfetch();
    syscall::print("\r\nSilver2Shell ready. Type 'help' for commands.\r\n");
    prompt();

    let mut line = [0_u8; LINE_CAPACITY];
    let mut length = 0_usize;
    loop {
        let Some(byte) = syscall::read() else {
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

fn execute(command: &str) {
    match command {
        "" => {}
        "help" => {
            syscall::print("Silver2Shell commands:\r\n");
            syscall::print("  help       Show this command list\r\n");
            syscall::print("  fastfetch  Show OS, uptime and memory information\r\n");
        }
        "fastfetch" => fastfetch(),
        _ => syscall::print("Unknown command. Type 'help'.\r\n"),
    }
}

fn fastfetch() {
    let uptime = syscall::uptime_ticks();
    let memory = syscall::memory_info();

    field(WOLF[0], "silver@silver2");
    field(WOLF[1], "---------------");
    field(WOLF[2], "OS: Silver Microkernel");
    field(WOLF[3], "Kernel: Rust / AArch64");
    field(WOLF[4], "Shell: Silver2Shell");

    syscall::print(WOLF[5]);
    syscall::print("Uptime: ");
    print_uptime(uptime);
    syscall::print("\r\n");

    syscall::print(WOLF[6]);
    syscall::print("Memory: ");
    if let Some(info) = memory {
        print_mib(info.used_bytes);
        syscall::print(" / ");
        print_mib(info.total_bytes);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    syscall::print(WOLF[7]);
    syscall::print("Free RAM: ");
    if let Some(info) = memory {
        print_mib(info.free_bytes);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");
}

fn field(logo: &str, text: &str) {
    syscall::print(logo);
    syscall::print(text);
    syscall::print("\r\n");
}

fn print_uptime(ticks: u64) {
    let seconds = ticks / TICKS_PER_SECOND;
    let hundredths = ticks % TICKS_PER_SECOND;
    syscall::print_dec_u64(seconds);
    syscall::print(".");
    if hundredths < 10 {
        syscall::print("0");
    }
    syscall::print_dec_u64(hundredths);
    syscall::print(" s");
}

fn print_mib(bytes: u64) {
    let tenths = bytes.saturating_mul(10) / MIB;
    syscall::print_dec_u64(tenths / 10);
    syscall::print(".");
    syscall::print_dec_u64(tenths % 10);
    syscall::print(" MiB");
}

fn prompt() {
    syscall::print("silver2> ");
}

fn echo(byte: u8) {
    let bytes = [byte];
    if let Ok(text) = core::str::from_utf8(&bytes) {
        syscall::print(text);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
