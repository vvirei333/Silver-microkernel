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
pub extern "C" fn _start(howler_id: u64) -> ! {
    syscall::print("\x1b[2J\x1b[H");
    fastfetch();
    syscall::print("\r\nhowl ready. Type 'help' for commands.\r\n");
    prompt();

    let mut line = [0_u8; LINE_CAPACITY];
    let mut length = 0_usize;
    loop {
        let Some(byte) = read_input(howler_id) else {
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

fn read_input(howler_id: u64) -> Option<u8> {
    if howler_id == 0 || !syscall::task_alive(howler_id) {
        return None;
    }

    let mut message = ipc::Message::new(0, &[]);
    match syscall::receive_timeout(&mut message, 0) {
        Ok(()) if message.sender == howler_id && message.payload[0] == 0 => {
            Some(message.payload[1])
        }
        _ => None,
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
    if command == "say" {
        run_utility(syscall::PROGRAM_SAY, "");
        return;
    }
    if let Some(arguments) = command.strip_prefix("say ") {
        run_utility(syscall::PROGRAM_SAY, unquote(arguments.trim()));
        return;
    }

    let mut words = command.split_whitespace();
    match words.next() {
        None => {}
        Some("help") => {
            syscall::print("howl commands:\r\n");
            syscall::print("  help       Show this command list\r\n");
            syscall::print("  fastfetch  Show OS, uptime and memory information\r\n");
            syscall::print("  ls [path]  List a directory (cwd by default)\r\n");
            syscall::print("  cat <file> Print a file\r\n");
            syscall::print("  echo <text> [> file | >> file]\r\n");
            syscall::print("             Print, truncate/write, or append to a file\r\n");
            syscall::print("  say <text> Print text\r\n");
            syscall::print("  sl         Run an animated steam locomotive\r\n");
            syscall::print("  mkdir <path>\r\n");
            syscall::print("  rm <file>  Remove a closed file\r\n");
            syscall::print("  rmdir <path>\r\n");
            syscall::print("             Remove an empty, unused directory\r\n");
            syscall::print("  mv <old> <new>\r\n");
            syscall::print("             Rename or move a closed file/unused directory\r\n");
            syscall::print("  cd <path>  Change current working directory\r\n");
            syscall::print("  pwd        Print current working directory\r\n");
            syscall::print("  clear      Clear the terminal screen\r\n");
            syscall::print("  uname      Print kernel name, version and architecture\r\n");
            syscall::print("  ps         List tasks and their states\r\n");
            syscall::print("  spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl> [arg]\r\n");
            syscall::print("  bg <program> [arg]\r\n");
            syscall::print("             Spawn without console ownership\r\n");
            syscall::print("  fg <cap>   Resume a task in the foreground\r\n");
            syscall::print("  Ctrl-Z     Stop the foreground task and return to shell\r\n");
            syscall::print("  kill <cap> Kill a task\r\n");
            syscall::print("  wait <cap> Wait for a task and print its exit status\r\n");
        }
        Some("fastfetch") => fastfetch(),
        Some("ls") => run_utility(syscall::PROGRAM_LS, words.next().unwrap_or("")),
        Some("cat") => run_utility(syscall::PROGRAM_CAT, words.next().unwrap_or("")),
        Some("sl") => run_utility(syscall::PROGRAM_SL, ""),
        Some("mkdir") => make_directory(words.next()),
        Some("rm") => remove_file(words.next()),
        Some("rmdir") => remove_directory(words.next()),
        Some("mv") => rename_node(words.next(), words.next()),
        Some("cd") => change_directory(words.next()),
        Some("pwd") => print_working_directory(),
        Some("clear") => clear_screen(),
        Some("uname") => syscall::print("Silver Microkernel v0.1.0 aarch64\r\n"),
        Some("ps") => show_tasks(),
        Some("spawn") | Some("bg") => spawn_program(words.next(), words.next()),
        Some("fg") => foreground_task(words.next()),
        Some("kill") => kill_task(words.next()),
        Some("wait") => wait_task(words.next()),
        Some(_) => syscall::print("Unknown command. Type 'help'.\r\n"),
    }
}

fn run_utility(program: u64, arguments: &str) {
    let Some(cap_id) = syscall::spawn_with_args(program, 0, arguments) else {
        syscall::print("Could not start program.\r\n");
        return;
    };
    if !syscall::console_set(cap_id) {
        syscall::print("Could not assign the foreground console.\r\n");
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
        None => syscall::print("Could not wait for program.\r\n"),
    }
}

fn execute_echo(arguments: &str) {
    let Ok((text, destination)) = split_redirection(arguments) else {
        syscall::print("echo: invalid quotes or redirection\r\n");
        return;
    };
    let text = unquote(text.trim());
    if let Some((path, append)) = destination {
        write_file(unquote(path.trim()), text, append);
    } else {
        run_utility(syscall::PROGRAM_ECHO, text);
    }
}

fn split_redirection(input: &str) -> Result<(&str, Option<(&str, bool)>), ()> {
    let mut quote = None;
    let mut redirect = None;
    for (index, byte) in input.bytes().enumerate() {
        match byte {
            b'\'' | b'"' if quote == Some(byte) => quote = None,
            b'\'' | b'"' if quote.is_none() => quote = Some(byte),
            b'>' if quote.is_none() => {
                if matches!(redirect, Some((previous, true)) if previous + 1 == index) {
                    continue;
                }
                if redirect.is_some() {
                    return Err(());
                }
                let append = input.as_bytes().get(index + 1) == Some(&b'>');
                redirect = Some((index, append));
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if let Some((index, append)) = redirect {
        let path_start = index + 1 + usize::from(append);
        let path = input[path_start..].trim();
        if path.is_empty() {
            return Err(());
        }
        Ok((&input[..index], Some((path, append))))
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

fn write_file(path: &str, text: &str, append: bool) {
    if path.is_empty() {
        syscall::print("echo: invalid destination\r\n");
        return;
    }
    let flags = if append {
        syscall::OPEN_WRITE | syscall::OPEN_APPEND
    } else {
        syscall::OPEN_WRITE
    };
    let fd = if append {
        // Append preserves existing files and creates only a missing one.
        let mut fd = syscall::fs_open(path, flags);
        if fd.is_none() {
            if !syscall::fs_create(path) {
                syscall::print("echo: cannot create file\r\n");
                return;
            }
            fd = syscall::fs_open(path, flags);
        }
        fd
    } else {
        // The '>' form retains its truncate-before-write behavior.
        if !syscall::fs_create(path) {
            syscall::print("echo: cannot create file\r\n");
            return;
        }
        syscall::fs_open(path, flags)
    };
    let Some(fd) = fd else {
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

fn remove_file(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: rm <file>\r\n");
        return;
    };
    if !syscall::fs_unlink(path) {
        syscall::print("rm: cannot remove file (missing, open, or a directory)\r\n");
    }
}

fn remove_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: rmdir <path>\r\n");
        return;
    };
    if !syscall::fs_rmdir(path) {
        syscall::print("rmdir: directory is missing, not empty, in use, or invalid\r\n");
    }
}

fn rename_node(source: Option<&str>, destination: Option<&str>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        syscall::print("Usage: mv <old> <new>\r\n");
        return;
    };
    if !syscall::fs_rename(source, destination) {
        syscall::print("mv: cannot rename (missing, same, open, busy, or bad destination)\r\n");
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

fn foreground_task(cap_id: Option<&str>) {
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

fn kill_task(cap_id: Option<&str>) {
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

fn wait_task(cap_id: Option<&str>) {
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
        syscall::PROGRAM_SILVERD => "silverd     ",
        syscall::PROGRAM_SCOUT => "scout       ",
        syscall::PROGRAM_HOWL => "howl        ",
        syscall::PROGRAM_HOWLER => "howler      ",
        syscall::PROGRAM_LS => "ls          ",
        syscall::PROGRAM_CAT => "cat         ",
        syscall::PROGRAM_ECHO => "echo        ",
        syscall::PROGRAM_SAY => "say         ",
        syscall::PROGRAM_SL => "sl          ",
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
        7 => "stopped",
        _ => "unknown",
    }
}

fn spawn_program(program: Option<&str>, argument: Option<&str>) {
    let Some(program) = program else {
        syscall::print(
            "Usage: spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl> [arg]\r\n",
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

    if let Some(cap_id) = syscall::spawn(program, argument) {
        if let Some(id) = syscall::cap_get_task_id(cap_id) {
            syscall::print("Spawned task ID=");
            syscall::print_dec_u64(id);
            syscall::print(" CapId=");
            syscall::print_dec_u64(cap_id);
            syscall::print("\r\n");
        } else {
            syscall::print("Spawned, but could not resolve CapId.\r\n");
        }
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
    field("silver@howl");
    field("---------------");
    field("OS: Silver Microkernel");
    field("Kernel: AArch64");
    field("Shell: howl");

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

fn clear_screen() {
    syscall::print("\x1b[2J\x1b[H");
}

fn prompt() {
    syscall::print("\x1b[1;36mhowl>\x1b[0m ");
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
