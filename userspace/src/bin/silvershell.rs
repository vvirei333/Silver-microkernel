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

const TASK_LIST_CAPACITY: usize = 32;
const EMPTY_TASK: syscall::TaskInfo = syscall::TaskInfo {
    id: 0,
    state: 0,
    program: 0,
    argument: 0,
};
const TASK_DEAD: u64 = 6;
const TASK_STOPPED: u64 = 7;
/// Re-resolves Howler once a second so a respawned driver is picked up and a
/// dead one is noticed without polling the task table on every keystroke.
const HOWLER_REFRESH_TICKS: u64 = TICKS_PER_SECOND;
/// A driver request is answered immediately; waiting two seconds keeps the
/// shell responsive when the driver died between send and reply.
const RESPONSE_TIMEOUT_TICKS: u64 = 2 * TICKS_PER_SECOND;
const RESPONSE_POLL_TICKS: u64 = 10;

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
    let mut howler = HowlerLink::new(howler_id);
    loop {
        let Some(byte) = read_input(&mut howler) else {
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

fn read_input(howler: &mut HowlerLink) -> Option<u8> {
    let howler_id = howler.resolve()?;

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
            syscall::print("  diskread [sector]\r\n");
            syscall::print("             Read a sector (0 by default), show first 16 bytes\r\n");
            syscall::print("  hexdump [sector]\r\n");
            syscall::print("             Dump 512 bytes of a sector (0 by default)\r\n");
            syscall::print("  sfsinfo    Show the mounted SFS superblock\r\n");
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
            syscall::print("  spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl|block_driver|sfs_server> [arg]\r\n");
            syscall::print("  bg <program> [arg]\r\n");
            syscall::print("             Spawn without console ownership\r\n");
            syscall::print("  fg <cap>   Resume a task in the foreground\r\n");
            syscall::print("  Ctrl-Z     Stop the foreground task and return to shell\r\n");
            syscall::print("  kill <cap> Kill a task\r\n");
            syscall::print("  wait <cap> Wait for a task and print its exit status\r\n");
        }
        Some("fastfetch") => fastfetch(),
        Some("diskread") => {
            let sector = match words.next() {
                Some(value) => match parse_number(value) {
                    Some(sector) => sector,
                    None => {
                        syscall::print("diskread: sector must be a non-negative integer\r\n");
                        return;
                    }
                },
                None => 0,
            };
            disk_read(sector);
        }
        Some("hexdump") => {
            let sector = match words.next() {
                Some(value) => match parse_number(value) {
                    Some(sector) => sector,
                    None => {
                        syscall::print("hexdump: sector must be a non-negative integer\r\n");
                        return;
                    }
                },
                None => 0,
            };
            hexdump(sector);
        }
        Some("sfsinfo") => sfs_info(),
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

/// Resolves the live task whose executable is `program`.
///
/// Task IDs depend on the spawn order of the whole boot, so nothing may assume
/// them. Dead and stopped tasks are skipped: a respawned driver must be found by
/// its program, not by a stale ID that still sits in the task table.
fn find_task_id(program: u64) -> Option<u64> {
    find_task_id_preferring(program, 0)
}

/// Same as [`find_task_id`], but uses `preferred` when that task is alive and
/// really runs `program`. A hint is never trusted on liveness alone.
///
/// The most recently spawned match wins. That matters for a hardware driver: a
/// respawned instance re-runs device setup and therefore owns the device, while
/// the older instance is left talking to a virtqueue the device no longer uses.
fn find_task_id_preferring(program: u64, preferred: u64) -> Option<u64> {
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

/// Locates the block driver for one request, printing `message` when it is not
/// running. Every call re-resolves the ID, so a driver that crashed and was
/// respawned under a new ID is used again without restarting the shell.
fn block_driver(message: &str) -> Option<u64> {
    let driver = find_task_id(syscall::PROGRAM_BLOCK_DRIVER);
    if driver.is_none() {
        syscall::print(message);
    }
    driver
}

fn disk_read(sector: u64) {
    static mut SHARED_MAPPING: Option<(u64, u64)> = None;

    let Some(driver) = block_driver("DISK DRIVER NOT RUNNING\r\n") else {
        return;
    };

    let request = ipc::Message::new(0, &sector.to_le_bytes());
    if !syscall::send(driver, &request) {
        syscall::print("DISK READ FAILED\r\n");
        return;
    }

    let mut response = ipc::Message::new(0, &[]);
    if !receive_from(driver, &mut response) {
        syscall::print("DISK READ FAILED\r\n");
        return;
    }
    match response.payload[0] {
        0 => {
            syscall::print("DISK READ FAILED\r\n");
            return;
        }
        2 => {
            syscall::print("DISK DRIVER NOT INITIALIZED (NO DISK?)\r\n");
            return;
        }
        1 => {}
        _ => {
            syscall::print("DISK READ FAILED\r\n");
            return;
        }
    }

    let mut physical_bytes = [0_u8; 8];
    let mut length_bytes = [0_u8; 2];
    physical_bytes.copy_from_slice(&response.payload[1..9]);
    length_bytes.copy_from_slice(&response.payload[9..11]);
    let physical_address = u64::from_le_bytes(physical_bytes);
    let length = usize::from(u16::from_le_bytes(length_bytes));
    if length != 512 {
        syscall::print("DISK READ FAILED\r\n");
        acknowledge_disk_buffer(driver);
        return;
    }

    let mapped = unsafe {
        match SHARED_MAPPING {
            Some((cached_physical, virtual_address)) if cached_physical == physical_address => {
                Some(virtual_address)
            }
            _ => {
                let mapped = syscall::map_phys(physical_address, length as u64);
                if let Some(virtual_address) = mapped {
                    SHARED_MAPPING = Some((physical_address, virtual_address));
                }
                mapped
            }
        }
    };
    let Some(virtual_address) = mapped else {
        syscall::print("DISK SHARED MEMORY MAP FAILED\r\n");
        acknowledge_disk_buffer(driver);
        return;
    };

    let mut data = [0_u8; 512];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile((virtual_address as *const u8).add(index)) };
    }
    acknowledge_disk_buffer(driver);

    let mut first = [0_u8; 8];
    let mut second = [0_u8; 8];
    first.copy_from_slice(&data[..8]);
    second.copy_from_slice(&data[8..16]);
    syscall::print("DISK SECTOR ");
    syscall::print_dec_u64(sector);
    syscall::print(": 0x");
    syscall::print_hex_u64(u64::from_be_bytes(first));
    syscall::print_hex_u64(u64::from_be_bytes(second));
    syscall::print("\r\n");
}

fn hexdump(sector: u64) {
    static mut SHARED_MAPPING: Option<(u64, u64)> = None;

    let Some(driver) = block_driver("HEXDUMP FAILED: DISK DRIVER NOT RUNNING\r\n") else {
        return;
    };

    let request = ipc::Message::new(0, &sector.to_le_bytes());
    if !syscall::send(driver, &request) {
        syscall::print("HEXDUMP FAILED\r\n");
        return;
    }

    let mut response = ipc::Message::new(0, &[]);
    if !receive_from(driver, &mut response) || response.payload[0] != 1 {
        syscall::print("HEXDUMP FAILED\r\n");
        return;
    }

    let mut physical_bytes = [0_u8; 8];
    let mut length_bytes = [0_u8; 2];
    physical_bytes.copy_from_slice(&response.payload[1..9]);
    length_bytes.copy_from_slice(&response.payload[9..11]);
    let physical_address = u64::from_le_bytes(physical_bytes);
    let length = usize::from(u16::from_le_bytes(length_bytes));

    if length != 512 {
        syscall::print("HEXDUMP FAILED: BAD LENGTH\r\n");
        acknowledge_disk_buffer(driver);
        return;
    }

    let mapped = unsafe {
        match SHARED_MAPPING {
            Some((cached_physical, virtual_address)) if cached_physical == physical_address => {
                Some(virtual_address)
            }
            _ => {
                let mapped = syscall::map_phys(physical_address, length as u64);
                if let Some(virtual_address) = mapped {
                    SHARED_MAPPING = Some((physical_address, virtual_address));
                }
                mapped
            }
        }
    };

    let Some(virtual_address) = mapped else {
        syscall::print("HEXDUMP FAILED: MAP FAILED\r\n");
        acknowledge_disk_buffer(driver);
        return;
    };

    let mut data = [0_u8; 512];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile((virtual_address as *const u8).add(index)) };
    }
    acknowledge_disk_buffer(driver);

    syscall::print("HEXDUMP SECTOR ");
    syscall::print_dec_u64(sector);
    syscall::print("\r\n");

    for line in 0..32 {
        let offset = line * 16;
        print_offset(offset);
        syscall::print("  ");
        for i in 0..16 {
            print_hex_byte(data[offset + i]);
            syscall::print(" ");
            if i == 7 {
                syscall::print(" ");
            }
        }
        syscall::print(" |");
        for i in 0..16 {
            let b = data[offset + i];
            let c = if (32..=126).contains(&b) { b } else { b'.' };
            let buf = [c];
            if let Ok(s) = core::str::from_utf8(&buf) {
                syscall::print(s);
            }
        }
        syscall::print("|\r\n");
    }
}

fn print_offset(offset: usize) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [b'0'; 8];
    let mut val = offset;
    for i in (0..8).rev() {
        buf[i] = HEX[val & 0xF];
        val >>= 4;
    }
    if let Ok(s) = core::str::from_utf8(&buf) {
        syscall::print(s);
    }
}

fn print_hex_byte(b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let buf = [HEX[(b >> 4) as usize], HEX[(b & 0xF) as usize]];
    if let Ok(s) = core::str::from_utf8(&buf) {
        syscall::print(s);
    }
}

/// Releases the driver's shared DMA window. Best effort: a driver that already
/// exited is not an error, its buffer is reclaimed with the task.
fn acknowledge_disk_buffer(driver: u64) {
    let acknowledgement = ipc::Message::new(0, &[0xA5]);
    let _ = syscall::send(driver, &acknowledgement);
}

/// Waits for a reply from `task_id` only, bounded so a driver that dies without
/// answering cannot block the shell forever. Unrelated queued messages are
/// discarded, which is fine: nothing else sends to the interactive shell.
fn receive_from(task_id: u64, buffer: &mut ipc::Message) -> bool {
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

/// Reads the SFS superblock through the filesystem server.
///
/// The server is resolved from the task table on every call, so it may be
/// respawned (or absent) without the shell caching a stale task ID.
fn sfs_info() {
    let Some(server) = find_task_id(syscall::PROGRAM_SFS_SERVER) else {
        syscall::print("SFS SERVER NOT RUNNING\r\n");
        return;
    };

    // Opcode 0 asks for the superblock metadata; the server answers its own
    // mounted copy, so the shell needs neither CAP_MMIO nor the block driver.
    // Temporary probe: a recognizable pattern shows exactly how the server
    // sees the request head (shift of 0, 1 or 8 bytes is distinguishable).
    // NOTE: payload[0] must be 0, not the sender ID, so the server can properly
    // set payload[0] = 1 (mounted) in its reply without echoing the sender ID.
    let mut request_payload = [0_u8; 32];
    request_payload[0] = 0;  // Ensure payload[0] is 0, not the sender ID
    request_payload[1] = 0x11;
    request_payload[2] = 0x22;
    request_payload[3] = 0x33;
    request_payload[4] = 0x44;
    request_payload[5] = 0x55;
    request_payload[6] = 0x66;
    request_payload[7] = 0x77;
    let request = ipc::Message::new(0, &request_payload);
    if !syscall::send(server, &request) {
        syscall::print("SFS SERVER NOT RESPONDING\r\n");
        return;
    }

    let mut response = ipc::Message::new(0, &[]);
    if !receive_from(server, &mut response) {
        syscall::print("SFS SERVER NOT RESPONDING\r\n");
        return;
    }
    if response.payload[0] != 1 {
        // Temporary probe: dump the whole reply, four little-endian 64-bit
        // words covering all 32 payload bytes, so the actual structure is
        // visible instead of guessed.
        syscall::print("SFS NOT MOUNTED\r\n");
        let mut offset = 0;
        while offset < 32 {
            let mut word = [0_u8; 8];
            word.copy_from_slice(&response.payload[offset..offset + 8]);
            syscall::print("  +");
            syscall::print_dec_u64(offset as u64);
            syscall::print("  0x");
            syscall::print_hex_u64(u64::from_le_bytes(word));
            syscall::print("\r\n");
            offset += 8;
        }
        return;
    }

    let mut block_size = [0_u8; 4];
    let mut total_blocks = [0_u8; 4];
    let mut root_inode = [0_u8; 4];
    let mut inode_count = [0_u8; 4];
    let mut magic = [0_u8; 4];
    block_size.copy_from_slice(&response.payload[1..5]);
    total_blocks.copy_from_slice(&response.payload[5..9]);
    root_inode.copy_from_slice(&response.payload[9..13]);
    inode_count.copy_from_slice(&response.payload[13..17]);
    magic.copy_from_slice(&response.payload[17..21]);

    let block_size = u32::from_le_bytes(block_size);
    let total_blocks = u32::from_le_bytes(total_blocks);

    syscall::print("SFS MOUNTED\r\n");
    syscall::print("  magic       0x");
    syscall::print_hex_u64(u64::from(u32::from_le_bytes(magic)));
    syscall::print("\r\n  block size  ");
    syscall::print_dec_u64(u64::from(block_size));
    syscall::print("\r\n  blocks      ");
    syscall::print_dec_u64(u64::from(total_blocks));
    syscall::print("\r\n  capacity    ");
    syscall::print_dec_u64(u64::from(block_size) * u64::from(total_blocks) / 1024);
    syscall::print(" KiB\r\n  root inode  ");
    syscall::print_dec_u64(u64::from(u32::from_le_bytes(root_inode)));
    syscall::print("\r\n  inodes      ");
    syscall::print_dec_u64(u64::from(u32::from_le_bytes(inode_count)));
    syscall::print("\r\n");
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
        syscall::PROGRAM_BLOCK_DRIVER => "block_driver",
        syscall::PROGRAM_SFS_SERVER => "sfs_server  ",
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

/// Hands a hand-spawned driver the rights the kernel does not grant by itself.
///
/// `CapTable::grant` requires the caller (the shell) to hold the right it passes
/// on, so only `CAP_MMIO` is delegated here — that is the entry `silvershell`
/// receives. `CAP_PCI` is granted by the kernel at spawn time for the block
/// driver and is deliberately not held by the shell.
///
/// Spawning still succeeds when the grant fails; the task simply runs without
/// device access, which is a diagnosable state rather than a silent one.
fn grant_device_rights(cap_id: u64, task_id: u64, program: u64) {
    let rights = match program {
        syscall::PROGRAM_BLOCK_DRIVER | syscall::PROGRAM_SFS_SERVER => syscall::CAP_MMIO,
        _ => return,
    };
    // Recipient must be the task ID: the CapId only names the shell's own entry
    // for that task, and the kernel resolves recipients by task ID.
    if !syscall::cap_grant(cap_id, task_id, rights) {
        syscall::print("Spawned, but CAP_MMIO grant failed: the task has no device access.\r\n");
    }
}

fn spawn_program(program: Option<&str>, argument: Option<&str>) {
    let Some(program) = program else {
        syscall::print(
            "Usage: spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl|block_driver|sfs_server> [arg]\r\n",
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
        "block_driver" => syscall::PROGRAM_BLOCK_DRIVER, // <--- ДОБАВЛЕНО
        "sfs_server" => syscall::PROGRAM_SFS_SERVER,     // <--- ДОБАВЛЕНО
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
            grant_device_rights(cap_id, id, program);
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
