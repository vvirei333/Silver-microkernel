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

const LOGO: &str = include_str!("../../../logo.txt");

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("\x1b[2J\x1b[H");
    fastfetch();
    syscall::print("\r\nsilvershell ready. Type 'help' for commands.\r\n");
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
    let command = command.trim();
    if command == "echo" {
        run_utility(syscall::PROGRAM_ECHO, "");
        return;
    }
    if let Some(arguments) = command.strip_prefix("echo ") {
        execute_echo(arguments);
        return;
    }

    let mut words = command.split_whitespace();
    match words.next() {
        None => {}
        Some("help") => {
            syscall::print("silvershell commands:\r\n");
            syscall::print("  help       Show this command list\r\n");
            syscall::print("  fastfetch  Show OS, uptime and memory information\r\n");
            syscall::print("  ls [path]  List a directory (cwd by default)\r\n");
            syscall::print("  cat <file> Print a file\r\n");
            syscall::print("  echo <text> [> file]\r\n");
            syscall::print("             Print text or truncate and write a file\r\n");
            syscall::print("  mkdir <path>\r\n");
            syscall::print("  cd <path>  Change current working directory\r\n");
            syscall::print("  pwd        Print current working directory\r\n");
            syscall::print("  ps         List tasks and their states\r\n");
            syscall::print("  spawn <demo|nameserver|silvershell|ls|cat|echo> [arg]\r\n");
            syscall::print("  kill <id>  Kill a task\r\n");
            syscall::print("  wait <id>  Wait for a task and print its exit status\r\n");
        }
        Some("fastfetch") => fastfetch(),
        Some("ls") => run_utility(syscall::PROGRAM_LS, words.next().unwrap_or("")),
        Some("cat") => run_utility(syscall::PROGRAM_CAT, words.next().unwrap_or("")),
        Some("mkdir") => make_directory(words.next()),
        Some("cd") => change_directory(words.next()),
        Some("pwd") => print_working_directory(),
        Some("ps") => show_tasks(),
        Some("spawn") => spawn_program(words.next(), words.next()),
        Some("kill") => kill_task(words.next()),
        Some("wait") => wait_task(words.next()),
        Some(_) => syscall::print("Unknown command. Type 'help'.\r\n"),
    }
}

fn run_utility(program: u64, arguments: &str) {
    let Some(id) = syscall::spawn_with_args(program, 0, arguments) else {
        syscall::print("Could not start program.\r\n");
        return;
    };
    match syscall::wait(id) {
        Some(0) => {}
        Some(status) => {
            syscall::print("Program exited with status ");
            syscall::print_dec_u64(status);
            syscall::print("\r\n");
        }
        None => syscall::print("Could not wait for program.\r\n"),
    }
}

fn execute_echo(arguments: &str) {
    let Ok((text, destination)) = split_redirection(arguments) else {
        syscall::print("echo: invalid quotes or redirection\r\n");
        return;
    };
    let text = unquote(text.trim());
    if let Some(path) = destination {
        write_file(unquote(path.trim()), text);
    } else {
        run_utility(syscall::PROGRAM_ECHO, text);
    }
}

fn split_redirection(input: &str) -> Result<(&str, Option<&str>), ()> {
    let mut quote = None;
    let mut redirect = None;
    for (index, byte) in input.bytes().enumerate() {
        match byte {
            b'\'' | b'"' if quote == Some(byte) => quote = None,
            b'\'' | b'"' if quote.is_none() => quote = Some(byte),
            b'>' if quote.is_none() => {
                if redirect.replace(index).is_some() {
                    return Err(());
                }
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if let Some(index) = redirect {
        let path = input[index + 1..].trim();
        if path.is_empty() {
            return Err(());
        }
        Ok((&input[..index], Some(path)))
    } else {
        Ok((input, None))
    }
}

fn unquote(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &text[1..text.len() - 1]
    } else {
        text
    }
}

fn write_file(path: &str, text: &str) {
    if path.is_empty() || !syscall::fs_create(path) {
        syscall::print("echo: cannot create file\r\n");
        return;
    }
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_WRITE) else {
        syscall::print("echo: cannot open file\r\n");
        return;
    };
    let ok = write_all(fd, text.as_bytes()) && write_all(fd, b"\r\n");
    syscall::fs_close(fd);
    if !ok {
        syscall::print("echo: write failed\r\n");
    }
}

fn write_all(fd: u64, bytes: &[u8]) -> bool {
    let mut written = 0;
    while written < bytes.len() {
        match syscall::fs_write(fd, &bytes[written..]) {
            Some(0) | None => return false,
            Some(count) => written += count,
        }
    }
    true
}

fn make_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: mkdir <path>\r\n");
        return;
    };
    if !syscall::fs_mkdir(path) {
        syscall::print("mkdir: cannot create directory\r\n");
    }
}

fn change_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: cd <path>\r\n");
        return;
    };
    if !syscall::fs_chdir(path) {
        syscall::print("cd: no such directory\r\n");
    }
}

fn print_working_directory() {
    let mut cwd = [0_u8; 192];
    let Some(len) = syscall::fs_getcwd(&mut cwd) else {
        syscall::print("pwd: cannot read current directory\r\n");
        return;
    };
    if let Ok(path) = core::str::from_utf8(&cwd[..len]) {
        syscall::print(path);
        syscall::print("\r\n");
    }
}

fn kill_task(id: Option<&str>) {
    let Some(id) = id.and_then(parse_number) else {
        syscall::print("Usage: kill <id>\r\n");
        return;
    };
    if syscall::kill(id) {
        syscall::print("Killed task ID=");
        syscall::print_dec_u64(id);
        syscall::print("\r\n");
    } else {
        syscall::print("kill: invalid task or task already exited\r\n");
    }
}

fn wait_task(id: Option<&str>) {
    let Some(id) = id.and_then(parse_number) else {
        syscall::print("Usage: wait <id>\r\n");
        return;
    };
    if let Some(status) = syscall::wait(id) {
        syscall::print("Task ID=");
        syscall::print_dec_u64(id);
        syscall::print(" exited with status ");
        syscall::print_dec_u64(status);
        syscall::print("\r\n");
    } else {
        syscall::print("wait: unknown task or status already consumed\r\n");
    }
}

fn show_tasks() {
    const EMPTY: syscall::TaskInfo = syscall::TaskInfo {
        id: 0,
        state: 0,
        program: 0,
        argument: 0,
    };
    let mut tasks = [EMPTY; 32];
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

fn program_name(program: u64) -> &'static str {
    match program {
        syscall::PROGRAM_DEMO => "demo        ",
        syscall::PROGRAM_NAMESERVER => "nameserver  ",
        syscall::PROGRAM_SILVERSHELL => "silvershell ",
        syscall::PROGRAM_LS => "ls          ",
        syscall::PROGRAM_CAT => "cat         ",
        syscall::PROGRAM_ECHO => "echo        ",
        _ => "kernel      ",
    }
}

fn state_name(state: u64) -> &'static str {
    match state {
        1 => "ready",
        2 => "running",
        3 => "sleeping",
        4 => "blocked",
        5 => "blocked-timeout",
        6 => "dead",
        _ => "unknown",
    }
}

fn spawn_program(program: Option<&str>, argument: Option<&str>) {
    let Some(program) = program else {
        syscall::print("Usage: spawn <demo|nameserver|silvershell|ls|cat|echo> [arg]\r\n");
        return;
    };
    let program = match program {
        "demo" => syscall::PROGRAM_DEMO,
        "nameserver" => syscall::PROGRAM_NAMESERVER,
        "silvershell" => syscall::PROGRAM_SILVERSHELL,
        "ls" => syscall::PROGRAM_LS,
        "cat" => syscall::PROGRAM_CAT,
        "echo" => syscall::PROGRAM_ECHO,
        _ => {
            syscall::print("Unknown program. See 'help' for the program list.\r\n");
            return;
        }
    };
    let argument = match argument {
        Some(text) => match parse_number(text) {
            Some(value) => value,
            None => {
                syscall::print("Argument must be a non-negative decimal integer.\r\n");
                return;
            }
        },
        None => 0,
    };

    if let Some(id) = syscall::spawn(program, argument) {
        syscall::print("Spawned task ID=");
        syscall::print_dec_u64(id);
        syscall::print("\r\n");
    } else {
        syscall::print("Spawn failed.\r\n");
    }
}

fn parse_number(text: &str) -> Option<u64> {
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

fn fastfetch() {
    let uptime = syscall::uptime_ticks();
    let memory = syscall::memory_info();

    print_logo();
    syscall::print("\r\n");
    field("silver@silvershell");
    field("---------------");
    field("OS: Silver Microkernel");
    field("Kernel: Rust / AArch64");
    field("Shell: silvershell");

    syscall::print("Uptime: ");
    print_uptime(uptime);
    syscall::print("\r\n");

    syscall::print("Memory: ");
    if let Some(info) = memory {
        print_mib(info.used_bytes);
        syscall::print(" / ");
        print_mib(info.total_bytes);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    syscall::print("Free RAM: ");
    if let Some(info) = memory {
        print_mib(info.free_bytes);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");
}

fn print_logo() {
    let start = LOGO
        .lines()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(0);
    let end = LOGO
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, _)| index + 1)
        .last()
        .unwrap_or(start);

    for line in LOGO.lines().skip(start).take(end - start) {
        syscall::print(line.trim_end());
        syscall::print("\r\n");
    }
}

fn field(text: &str) {
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
    syscall::print("silvershell> ");
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
