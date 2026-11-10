#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

/// One command line. 256 bytes are needed for `echo sig=<128 hex> >> <path>`;
/// the older 96-byte limit silently truncated signing commands.
const LINE_CAPACITY: usize = 256;
const TICKS_PER_SECOND: u64 = 100;
const MIB: u64 = 1024 * 1024;
/// IPC opcode for a button press reported by `gpu_driver` (the shell owns the
/// console, so it prints `GUI: Button '<name>' pressed!`). Matches the value in
/// `gpu_driver.rs`.
const GUI_BUTTON_PRESSED: u8 = 2;

/// On-disk SFS server opcodes for the write path (`sfsinfo` uses opcode 0).
const SFS_OP_TOUCH: u8 = 1;
const SFS_OP_WRITE_FILE: u8 = 2;
const SFS_OP_READ_FILE: u8 = 3;
/// Longest filename and combined name+data the single-shot SFS IPC carries.
/// Full-path bound, not a component bound: `/sfs/apps/scopeprobe.manifest`
/// is longer than one name yet perfectly valid.
const SFS_PATH_MAX: usize = 191;
const SFS_NAME_DATA_LIMIT: usize = 29;

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
    syscall::register("howl");
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
    let Some(howler_id) = howler.resolve() else {
        // No userspace UART driver (for example the first Redmi 10C boot,
        // where only Scout and Howl exist). Howl owns the console, so the
        // kernel console is a legal non-blocking input source.
        return syscall::read();
    };

    let mut message = ipc::Message::new(0, &[]);
    while syscall::receive_timeout(&mut message, 0).is_ok() {
        if message.sender == howler_id && message.payload[0] == 0 {
            return Some(message.payload[1]);
        }
        if message.payload[0] == GUI_BUTTON_PRESSED {
            report_gui_press(&message.payload[1..]);
        }
    }
    None
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

    if let Some(name) = command.strip_prefix("linuxrun-") {
        run_linux_alias(name);
        return;
    }

    let mut words = command.split_whitespace();
    match words.next() {
        None => {}
        Some("help") => {
            syscall::print("howl commands:\r\n");
            syscall::print("  help       Show this command list\r\n");
            syscall::print("  fastfetch  Show the aligned system summary\r\n");
            syscall::print("  sfsinfo    Show the mounted SFS superblock\r\n");
            syscall::print("  memtest    mmap 1 MiB, write/read it back\r\n");
            syscall::print("  ping <ip>  Send one ICMP echo request\r\n");
            syscall::print("  ip         Show the IPv4 configuration (DHCP or static)\r\n");
            syscall::print("  dns <name> Resolve a host name through the network server\r\n");
            syscall::print("  wget <url> [file]\r\n");
            syscall::print("             Fetch http:// into a file (/tmp/index.html)\r\n");
            syscall::print("  wolfser [query|url]\r\n");
            syscall::print("             Open Wolfser: search bar and host window (ivshmem)\r\n");
            syscall::print("  ls [path]  List a directory (cwd by default)\r\n");
            syscall::print("  cat <file> Print a file\r\n");
            syscall::print("  run <path>  Load and run an ELF file from the filesystem\r\n");
            #[cfg(feature = "linux-abi")]
            syscall::print("  linuxrun <path>  Run a Linux ABI ELF (x8 = Linux syscall)\r\n");
            syscall::print("  linuxrun-<app>   Run /sfs/apps/<app> or /sfs/<app>\r\n");
            syscall::print("  echo <text> [> file | >> file]\r\n");
            syscall::print("             Print, truncate/write, or append to a file\r\n");
            syscall::print("  touch <file>\r\n");
            syscall::print("             Create an empty file in the on-disk SFS\r\n");
            syscall::print("  say <text> Print text\r\n");
            syscall::print("  sl         Run an animated steam locomotive\r\n");
            syscall::print("  mkdir <path>\r\n");
            syscall::print("  rm <file>  Remove a closed file\r\n");
            syscall::print("  rmdir <path>\r\n");
            syscall::print("             Remove an empty, unused directory\r\n");
            syscall::print("  mv <old> <new>\r\n");
            syscall::print("             Rename or move a closed file/unused directory\r\n");
            syscall::print("  replace <src> <dst>\r\n");
            syscall::print("             Atomically replace dst with src (files)\r\n");
            syscall::print("  cd <path>  Change current working directory\r\n");
            syscall::print("  pwd        Print current working directory\r\n");
            syscall::print("  clear      Clear the terminal screen\r\n");
            syscall::print("  uname      Print kernel name, version and architecture\r\n");
            syscall::print("  health     Kernel resource and leak snapshot\r\n");
            syscall::print("  du [path]  SFS inode/block usage of a subtree\r\n");
            syscall::print("  ps         List tasks and their states\r\n");
            syscall::print("  spawn <demo|silverd|scout|howl|howler|ls|cat|echo|say|sl|block_driver|sfs_server> [arg]\r\n");
            syscall::print("  bg <program> [arg]\r\n");
            syscall::print("             Spawn without console ownership\r\n");
            syscall::print("  fg <cap>   Resume a task in the foreground\r\n");
            syscall::print("  Ctrl-Z     Stop the foreground task and return to shell\r\n");
            syscall::print("  kill <cap> Kill a task\r\n");
            syscall::print("  wait <cap> Wait for a task and print its exit status\r\n");
            syscall::print("  poweroff   Power the machine down through PSCI\r\n");
            syscall::print("  reboot     Reset the machine through PSCI\r\n");
            syscall::print("  grep <pattern> <file> | wc <file> | head/tail [n] <file>\r\n");
            syscall::print("  cp <src> <dst> | find [path] | seq N | basename/dirname <path>\r\n");
            syscall::print("  true | false | sleep <seconds>\r\n");
        }
        Some("fastfetch") => fastfetch(),
        Some("sfsinfo") => sfs_info(),
        Some("memtest") => memtest(),
        Some("ping") => ping_command(words.next()),
        Some("ip") | Some("ifconfig") => ip_command(),
        Some("dns") => dns_command(words.next()),
        Some("wget") => wget_command(words.next(), words.next()),
        Some("wolfser") | Some("browser") => wolfser_command(words.next()),
        Some("ls") => run_utility(syscall::PROGRAM_LS, words.next().unwrap_or("")),
        Some("bigdir") => run_utility(syscall::PROGRAM_BIGDIR, words.next().unwrap_or("")),
        Some("bigfile") => run_utility(syscall::PROGRAM_BIGFILE, ""),
        Some("cat") => run_utility(syscall::PROGRAM_CAT, words.next().unwrap_or("")),
        Some("touch") => touch_command(words.next()),
        Some("run") => run_elf(words.next().unwrap_or("")),
        #[cfg(feature = "linux-abi")]
        Some("linuxrun") => run_linux_elf(words.next().unwrap_or("")),
        Some("sl") => run_utility(syscall::PROGRAM_SL, ""),
        Some("mkdir") => make_directory(words.next()),
        Some("rm") => remove_file(words.next()),
        Some("rmdir") => remove_directory(words.next()),
        Some("mv") => rename_node(words.next(), words.next()),
        Some("replace") => replace_node(words.next(), words.next()),
        Some("cd") => change_directory(words.next()),
        Some("pwd") => print_working_directory(),
        Some("clear") => clear_screen(),
        Some("uname") => syscall::print("Silver Kernel v0.1.1 aarch64 (WerewolfOS)\r\n"),
        Some("health") => show_health(),
        Some("du") => show_usage(words.next()),
        Some("ps") => show_tasks(),
        Some("spawn") | Some("bg") => spawn_program(words.next(), words.next()),
        Some("fg") => foreground_task(words.next()),
        Some("kill") => kill_task(words.next()),
        Some("wait") => wait_task(words.next()),
        Some("poweroff") => syscall::power_off(),
        Some("reboot") => syscall::power_reset(),
        Some("grep") | Some("wc") | Some("head") | Some("tail") | Some("cp")
        | Some("find") | Some("seq") | Some("basename") | Some("dirname")
        | Some("true") | Some("false") | Some("sleep") => {
            run_utility(syscall::PROGRAM_UTILS, command);
        }
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

/// Sends one ICMP echo through the network server.
///
/// The server owns smoltcp and the driver, so the shell only ships the parsed
/// address and waits for the outcome. The server answers "timeout" after its
/// own two-second wait, so the shell's deadline must exceed that: three
/// seconds leaves a full second for scheduler latency on the reply.
fn ping_command(argument: Option<&str>) {
    const PING_WAIT_TICKS: u64 = 3 * TICKS_PER_SECOND;

    let Some(argument) = argument else {
        syscall::print("usage: ping <a.b.c.d>\r\n");
        return;
    };
    let Some(address) = parse_ipv4(argument) else {
        syscall::print("ping: address must be a.b.c.d with octets 0-255\r\n");
        return;
    };
    let Some(server) = find_task_id(syscall::PROGRAM_NET_SERVER) else {
        syscall::print("NET SERVER NOT RUNNING\r\n");
        return;
    };
    if !syscall::send(server, &net_ipc::ping(address)) {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    }

    let deadline = syscall::uptime_ticks().saturating_add(PING_WAIT_TICKS);
    let mut response = ipc::Message::new(0, &[]);
    let replied = loop {
        if !syscall::task_alive(server) {
            break false;
        }
        match syscall::receive_timeout(&mut response, RESPONSE_POLL_TICKS) {
            Ok(()) if response.sender == server => break true,
            Ok(()) => {}
            Err(_) if syscall::uptime_ticks() >= deadline => break false,
            Err(_) => {}
        }
    };
    if !replied {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    }
    syscall::print("PING ");
    print_dotted(address);
    syscall::print(": ");
    match net_ipc::ping_reply_fields(&response) {
        Some((replied, true)) if replied == address => syscall::print("OK\r\n"),
        _ => syscall::print("TIMEOUT\r\n"),
    }
}

/// Parses a strict dotted-quad IPv4 address: four decimal octets, no spaces,
/// every octet 0..=255.
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut address = [0_u8; 4];
    let mut octets = text.split('.');
    for octet in address.iter_mut() {
        let value = octets.next()?;
        if value.is_empty() || value.len() > 3 {
            return None;
        }
        let mut number = 0_u16;
        for byte in value.bytes() {
            if !byte.is_ascii_digit() {
                return None;
            }
            number = number * 10 + u16::from(byte - b'0');
        }
        if number > 255 {
            return None;
        }
        *octet = number as u8;
    }
    if octets.next().is_some() {
        return None;
    }
    Some(address)
}

fn print_dotted(address: [u8; 4]) {
    for (index, octet) in address.iter().enumerate() {
        if index > 0 {
            syscall::print(".");
        }
        syscall::print_dec_u64(u64::from(*octet));
    }
}

// ---------------------------------------------------------------------------
// Network client helpers
//
// The network server owns smoltcp. Everything larger than a 32-byte IPC
// payload travels through the server's DMA window, which the shell maps once
// per operation with `map_phys`.

/// DNS and configuration replies are local and quick; HTTP gets a real
/// transfer budget.
const NET_LOOKUP_TICKS: u64 = 10 * TICKS_PER_SECOND;
const NET_HTTP_TICKS: u64 = 40 * TICKS_PER_SECOND;

/// Waits for one reply opcode from `server`, discarding unrelated traffic.
fn net_wait_reply(server: u64, opcode: u8, timeout_ticks: u64) -> Option<ipc::Message> {
    let deadline = syscall::uptime_ticks().saturating_add(timeout_ticks);
    let mut response = ipc::Message::new(0, &[]);
    loop {
        if !syscall::task_alive(server) {
            return None;
        }
        match syscall::receive_timeout(&mut response, RESPONSE_POLL_TICKS) {
            Ok(()) if response.sender == server && response.payload[0] == opcode => {
                return Some(response)
            }
            Ok(()) => {}
            Err(_) if syscall::uptime_ticks() >= deadline => return None,
            Err(_) => {}
        }
    }
}

/// Cached mapping of the server's data window. The shell is single threaded,
/// and re-mapping 128 KiB per command would slowly exhaust the per-task MMIO
/// capability table.
static mut NET_WINDOW: Option<(u64, u64, u64)> = None;

/// Maps the server's shared data window into this task (and caches it).
fn net_window(server: u64) -> Option<u64> {
    if !syscall::send(server, &net_ipc::share_request()) {
        syscall::print("net: share send failed\r\n");
        return None;
    }
    let Some(reply) = net_wait_reply(server, net_ipc::OP_SHARE_REPLY, RESPONSE_TIMEOUT_TICKS)
    else {
        syscall::print("net: no share reply\r\n");
        return None;
    };
    let Some((paddr, size)) = net_ipc::share_fields(&reply) else {
        syscall::print("net: bad share fields\r\n");
        return None;
    };
    let cached = unsafe { *core::ptr::addr_of!(NET_WINDOW) };
    if let Some((cached_paddr, cached_size, vaddr)) = cached {
        if cached_paddr == paddr && cached_size == size {
            return Some(vaddr);
        }
    }
    let Some(vaddr) = syscall::map_phys(paddr, size) else {
        syscall::print("net: map failed\r\n");
        return None;
    };
    unsafe {
        *core::ptr::addr_of_mut!(NET_WINDOW) = Some((paddr, size, vaddr));
    }
    Some(vaddr)
}

/// Writes a length-prefixed request string into the shared window.
fn write_window_request(base: u64, text: &str) -> bool {
    if text.is_empty() || text.len() > net_ipc::CLIENT_REQUEST_MAX {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(base as *mut u16, text.len() as u16);
        core::ptr::copy_nonoverlapping(
            text.as_ptr(),
            (base + net_ipc::CLIENT_REQUEST + 2) as *mut u8,
            text.len(),
        );
    }
    net_ipc::fence();
    true
}

fn net_error_text(code: u8) -> &'static str {
    match code {
        net_ipc::NET_ERR_DNS => "name resolution failed",
        net_ipc::NET_ERR_CONNECT => "connection failed",
        net_ipc::NET_ERR_TIMEOUT => "timed out",
        net_ipc::NET_ERR_URL => "invalid URL",
        net_ipc::NET_ERR_TLS => "TLS handshake or certificate verification failed",
        net_ipc::NET_ERR_RESPONSE => "malformed HTTP response (or no A record)",
        net_ipc::NET_ERR_BUSY => "network server busy",
        net_ipc::NET_REDIRECT => "redirect (open it in wolfser)",
        _ => "network error",
    }
}

/// `ip` / `ifconfig`: show the address the network server is using.
fn ip_command() {
    let Some(server) = find_task_id(syscall::PROGRAM_NET_SERVER) else {
        syscall::print("NET SERVER NOT RUNNING\r\n");
        return;
    };
    if !syscall::send(server, &net_ipc::ipconfig_request()) {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    }
    let Some(reply) = net_wait_reply(server, net_ipc::OP_IPCONFIG_REPLY, RESPONSE_TIMEOUT_TICKS)
    else {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    };
    let Some((configured, address, prefix, gateway, dns)) =
        net_ipc::ipconfig_fields(&reply)
    else {
        return;
    };
    syscall::print(if configured {
        "eth0: DHCP lease\r\n"
    } else {
        "eth0: static fallback\r\n"
    });
    syscall::print("  inet ");
    print_dotted(address);
    syscall::print("/");
    syscall::print_dec_u64(u64::from(prefix));
    syscall::print("\r\n  gateway ");
    print_dotted(gateway);
    syscall::print("\r\n  dns     ");
    print_dotted(dns);
    syscall::print("\r\n");
}

/// `dns <name>`: resolve one name through the server's DNS socket.
fn dns_command(name: Option<&str>) {
    let Some(name) = name else {
        syscall::print("usage: dns <name>\r\n");
        return;
    };
    let Some(server) = find_task_id(syscall::PROGRAM_NET_SERVER) else {
        syscall::print("NET SERVER NOT RUNNING\r\n");
        return;
    };
    let Some(base) = net_window(server) else {
        syscall::print("dns: cannot map the network window\r\n");
        return;
    };
    if !write_window_request(base, name) {
        syscall::print("dns: name is too long\r\n");
        return;
    }
    if !syscall::send(server, &net_ipc::dns_request()) {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    }
    let Some(reply) = net_wait_reply(server, net_ipc::OP_DNS_REPLY, NET_LOOKUP_TICKS) else {
        syscall::print("dns: no reply from the network server\r\n");
        return;
    };
    let Some((code, address)) = net_ipc::dns_fields(&reply) else {
        return;
    };
    if code == net_ipc::NET_OK {
        syscall::print(name);
        syscall::print(" -> ");
        print_dotted(address);
        syscall::print("\r\n");
    } else {
        syscall::print("dns: ");
        syscall::print(net_error_text(code));
        syscall::print("\r\n");
    }
}

/// `wget <url> [file]`: fetch an `http://` document into SFS.
/// `wolfser [query|url]`: starts Wolfser in the background. Without an
/// argument it opens with an empty search bar, which is the normal way in.
fn wolfser_command(url: Option<&str>) {
    if find_task_id(syscall::PROGRAM_WOLFSER).is_some() {
        syscall::print("wolfser: already running in the host window\r\n");
        return;
    }
    let url = url.unwrap_or("");
    let Some(cap_id) = syscall::spawn_with_args(syscall::PROGRAM_WOLFSER, 0, url) else {
        syscall::print("wolfser: cannot start the program\r\n");
        return;
    };
    syscall::print("wolfser: started (host viewer shows the window)\r\n");
    let _ = cap_id;
}

fn wget_command(url: Option<&str>, destination: Option<&str>) {
    let Some(url) = url else {
        syscall::print("usage: wget <http(s)://host/path> [file]\r\n");
        return;
    };
    let destination = destination.unwrap_or("/tmp/index.html");
    if destination.len() > SFS_PATH_MAX {
        syscall::print("wget: destination path is too long\r\n");
        return;
    }
    let Some(server) = find_task_id(syscall::PROGRAM_NET_SERVER) else {
        syscall::print("NET SERVER NOT RUNNING\r\n");
        return;
    };
    let Some(base) = net_window(server) else {
        syscall::print("wget: cannot map the network window\r\n");
        return;
    };
    if !write_window_request(base, url) {
        syscall::print("wget: URL is too long\r\n");
        return;
    }
    syscall::print("wget: fetching ");
    syscall::print(url);
    syscall::print("\r\n");
    if !syscall::send(server, &net_ipc::http_request()) {
        syscall::print("NET SERVER NOT RESPONDING\r\n");
        return;
    }
    let Some(reply) = net_wait_reply(server, net_ipc::OP_HTTP_REPLY, NET_HTTP_TICKS) else {
        syscall::print("wget: no reply from the network server\r\n");
        return;
    };
    let Some((code, status, body_len)) = net_ipc::http_fields(&reply) else {
        return;
    };
    if code != net_ipc::NET_OK {
        syscall::print("wget: ");
        syscall::print(net_error_text(code));
        if status != 0 {
            syscall::print(" (stage ");
            syscall::print_dec_u64(u64::from(status));
            syscall::print(")");
        }
        syscall::print("\r\n");
        return;
    }
    let _ = syscall::fs_create(destination);
    let Some(fd) = syscall::fs_open(destination, syscall::OPEN_WRITE) else {
        syscall::print("wget: cannot open the destination\r\n");
        return;
    };
    // FS write is capped at 4 KiB per syscall, so stream the body in chunks.
    // SFS caps a file at 512 KiB; a larger page is deliberately truncated
    // instead of making every write fail silently.
    const WGET_LIMIT: usize = 512 * 1024;
    let available = body_len as usize;
    let keep = available.min(WGET_LIMIT);
    let body = unsafe {
        core::slice::from_raw_parts(
            (base + net_ipc::CLIENT_BODY) as *const u8,
            keep,
        )
    };
    let mut written = 0usize;
    while written < body.len() {
        let end = (written + 4096).min(body.len());
        match syscall::fs_write(fd, &body[written..end]) {
            Some(count) if count > 0 => written += count,
            _ => {
                let _ = syscall::fs_close(fd);
                syscall::print("wget: write failed\r\n");
                return;
            }
        }
    }
    let _ = syscall::fs_close(fd);
    syscall::print("wget: HTTP ");
    syscall::print_dec_u64(u64::from(status));
    syscall::print(", ");
    syscall::print_dec_u64(keep as u64);
    syscall::print(" bytes -> ");
    syscall::print(destination);
    if available > keep {
        syscall::print(" (truncated from ");
        syscall::print_dec_u64(available as u64);
        syscall::print(")");
    }
    syscall::print("\r\n");
}

/// Reads the SFS superblock through the filesystem server.
///
/// The server is resolved from the task table on every call, so it may be
/// respawned (or absent) without the shell caching a stale task ID.
fn sfs_info() {
    syscall::print("SFS: mounted by the kernel at /sfs\r\n");
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

/// Loads an ELF from the filesystem (`run <path>`) and runs it in the
/// foreground: the kernel reads the file, loads it and spawns it, then the
/// shell lends it the console and waits — mirroring `run_utility`, but with the
/// executable resolved from the VFS instead of the embedded program table.
fn run_elf(path: &str) {
    run_elf_with(path, syscall::spawn_path, true);
}

/// Loads a foreign ELF (`linuxrun <path>`) and runs it with the Linux syscall
/// personality. The kernel assigns the console inside the spawn, before the
/// child can run on any CPU, so no `console_set` round trip is needed.
#[cfg(feature = "linux-abi")]
fn run_linux_elf(path: &str) {
    run_elf_with(path, syscall::spawn_linux_path, false);
}

/// `linuxrun-<app>`: resolves `/sfs/apps/<app>` then `/sfs/<app>`, so a
/// installed bundle and a plain test binary share one short command.
fn run_linux_alias(name: &str) {
    run_elf_with(name, spawn_linux_alias, false);
}

fn spawn_linux_alias(name: &str) -> Option<u64> {
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

fn run_elf_with(path: &str, spawn: fn(&str) -> Option<u64>, assign_console: bool) {
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
    if path.is_empty() || path.len() > SFS_PATH_MAX {
        syscall::print("echo: invalid destination\r\n");
        return;
    }
    let flags = if append {
        syscall::OPEN_WRITE | syscall::OPEN_APPEND
    } else {
        syscall::OPEN_WRITE
    };
    // `>` truncates even an existing file, so create first; `>>` only creates
    // when the file is missing.
    if !append {
        let _ = syscall::fs_create(path);
    }
    let fd = match syscall::fs_open(path, flags) {
        Some(fd) => Some(fd),
        None => {
            let _ = syscall::fs_create(path);
            syscall::fs_open(path, flags)
        }
    };
    let Some(fd) = fd else {
        syscall::print("echo: cannot open destination\r\n");
        return;
    };
    // Unix `echo` terminates the line. Without it, `echo a >> f; echo b >> f`
    // produces one concatenated line and every line-oriented tool (grep, wc,
    // head, tail) sees a single blob.
    let mut wrote = match syscall::fs_write(fd, text.as_bytes()) {
        Some(count) => count,
        None => {
            syscall::print("echo: write failed\r\n");
            let _ = syscall::fs_close(fd);
            return;
        }
    };
    if syscall::fs_write(fd, b"\n").is_some() {
        wrote += 1;
    }
    syscall::print("SFS: wrote ");
    syscall::print_dec_u64(wrote as u64);
    syscall::print(" bytes\r\n");
    let _ = syscall::fs_close(fd);
}

fn touch_command(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: touch <file>\r\n");
        return;
    };
    if path.is_empty() || path.len() > SFS_PATH_MAX {
        syscall::print("touch: invalid path\r\n");
        return;
    }
    if let Some(fd) = syscall::fs_open(path, 0) {
        let _ = syscall::fs_close(fd);
        syscall::print("touch: already exists\r\n");
        return;
    }
    if syscall::fs_create(path) {
        syscall::print("touch: created\r\n");
    } else {
        syscall::print("touch: failed\r\n");
    }
}

fn memtest() {
    const SIZE: usize = 1024 * 1024;
    let Some(base) = syscall::mmap(0, SIZE, syscall::PROT_READ | syscall::PROT_WRITE) else {
        syscall::print("memtest: mmap failed\r\n");
        return;
    };
    syscall::print("memtest: mapped 1 MiB at 0x");
    syscall::print_hex_u64(base as u64);
    syscall::print("\r\n");

    let pointer = base as *mut u8;
    let message = b"Silver memtest!";
    for (index, &byte) in message.iter().enumerate() {
        unsafe { core::ptr::write_volatile(pointer.add(index), byte) };
    }

    let mut round_trip = true;
    for (index, &byte) in message.iter().enumerate() {
        if unsafe { core::ptr::read_volatile(pointer.add(index)) } != byte {
            round_trip = false;
        }
    }
    syscall::print(if round_trip {
        "memtest: string round-trip OK (\"Silver memtest!\")\r\n"
    } else {
        "memtest: string MISMATCH\r\n"
    });

    for index in 0..SIZE {
        unsafe { core::ptr::write_volatile(pointer.add(index), (index & 0xFF) as u8) };
    }
    let mut pattern_ok = true;
    for index in 0..SIZE {
        if unsafe { core::ptr::read_volatile(pointer.add(index)) } != (index & 0xFF) as u8 {
            pattern_ok = false;
            break;
        }
    }
    syscall::print(if pattern_ok {
        "memtest: 1 MiB pattern OK\r\n"
    } else {
        "memtest: 1 MiB pattern MISMATCH\r\n"
    });

    if syscall::munmap(base, SIZE) {
        syscall::print("memtest: unmapped\r\n");
    } else {
        syscall::print("memtest: munmap failed\r\n");
    }
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

fn replace_node(source: Option<&str>, destination: Option<&str>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        syscall::print("Usage: replace <source> <destination>\r\n");
        return;
    };
    if !syscall::fs_replace(source, destination) {
        syscall::print("replace: cannot replace (missing, open, busy, or bad path)\r\n");
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

fn show_usage(path: Option<&str>) {
    let path = path.unwrap_or(".");
    let Some(usage) = syscall::fs_usage(path) else {
        syscall::print("du: cannot account for that path\r\n");
        return;
    };
    syscall::print(path);
    syscall::print(": inodes ");
    syscall::print_dec_u64(usage.inodes);
    syscall::print(", blocks ");
    syscall::print_dec_u64(usage.blocks);
    syscall::print(" (data ");
    syscall::print_dec_u64(usage.data_blocks);
    syscall::print(", metadata ");
    syscall::print_dec_u64(usage.metadata_blocks);
    syscall::print("), bytes ");
    syscall::print_dec_u64(usage.blocks.saturating_mul(512));
    syscall::print("\r\n");
}

fn show_health() {
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
        syscall::PROGRAM_UTILS => "utils       ",
        syscall::PROGRAM_NET_DRIVER => "net_driver  ",
        syscall::PROGRAM_NET_SERVER => "net_server  ",
        syscall::PROGRAM_DYNAMIC => "elf         ",
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

fn spawn_program(program: Option<&str>, argument: Option<&str>) {
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
    let health = syscall::health_info();
    let disk = syscall::fs_usage("/sfs");

    print_logo();
    syscall::print("\r\n");
    plain("werewolf@howl");
    plain("-------------");
    field("OS:", "WerewolfOS 0.1.1");
    field("Kernel:", "Silver Microkernel 0.1.1");
    field("Codename:", "First Howl");
    field("Arch:", "AArch64");
    field("Platform:", "QEMU / ARM64");
    field("Type:", "Microkernel");
    field("Shell:", "Howl");
    field("FS:", "SFS");
    field("Status:", "Experimental");

    heading("Uptime:");
    print_uptime(uptime);
    syscall::print("\r\n");

    heading("Memory:");
    if let Some(info) = memory {
        print_mib(info.used_bytes);
        syscall::print(" / ");
        print_mib(info.total_bytes);
        syscall::print(" (");
        print_percent(info.used_bytes, info.total_bytes);
        syscall::print(")");
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    heading("Free RAM:");
    if let Some(info) = memory {
        print_mib(info.free_bytes);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    heading("Processes:");
    if let Some(info) = health {
        syscall::print_dec_u64(info.live_tasks);
        syscall::print(" live / ");
        syscall::print_dec_u64(info.zombies);
        syscall::print(" zombie");
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    heading("Open files:");
    if let Some(info) = health {
        syscall::print_dec_u64(info.open_files);
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    heading("Disk (/sfs):");
    if let Some(usage) = disk {
        print_size(usage.blocks.saturating_mul(512));
        syscall::print(" used / ");
        syscall::print_dec_u64(usage.inodes);
        syscall::print(" inodes");
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    heading("DMA:");
    if let Some(info) = health {
        syscall::print_dec_u64(info.dma_leaked_pages);
        syscall::print(" leaked / ");
        syscall::print_dec_u64(info.dma_quarantined_pages);
        syscall::print(" quarantined");
    } else {
        syscall::print("unavailable");
    }
    syscall::print("\r\n");

    print_color_bar();
}

/// Fastfetch prints its labels as the left column and values as the right.
/// Padding to a fixed column keeps the numbers aligned instead of the old
/// `OS: ...` one-string-per-line style.
const FIELD_WIDTH: usize = 13;

fn plain(text: &str) {
    syscall::print(text);
    syscall::print("\r\n");
}

fn heading(label: &str) {
    syscall::print(label);
    let mut column = label.len();
    while column < FIELD_WIDTH {
        syscall::print(" ");
        column += 1;
    }
}

fn field(label: &str, value: &str) {
    heading(label);
    syscall::print(value);
    syscall::print("\r\n");
}

/// A 16-colour strip, like the one at the bottom of fastfetch. Two spaces per
/// block keeps it readable on the 80-column serial console.
fn print_color_bar() {
    syscall::print("\r\n");
    syscall::print(
        "\x1b[40m  \x1b[41m  \x1b[42m  \x1b[43m  \x1b[44m  \x1b[45m  \x1b[46m  \x1b[47m  \x1b[0m\r\n",
    );
    syscall::print(
        "\x1b[100m  \x1b[101m  \x1b[102m  \x1b[103m  \x1b[104m  \x1b[105m  \x1b[106m  \x1b[107m  \x1b[0m\r\n",
    );
}

fn print_percent(used: u64, total: u64) {
    let percent = if total == 0 {
        0
    } else {
        used.saturating_mul(100) / total
    };
    syscall::print_dec_u64(percent);
    syscall::print("%");
}

fn print_size(bytes: u64) {
    if bytes >= MIB {
        print_mib(bytes);
    } else {
        syscall::print_dec_u64(bytes / 1024);
        syscall::print(" KiB");
    }
}

fn print_logo() {
    // Deliberately no `Lines`/`trim` iterator chain here. The optimizer used
    // to leave the iterator's `start` field in an unwritten stack slot, and on
    // some boots it picked up a stale pointer; `str::trim` then read it and
    // killed the shell before the prompt (intermittent since the userspace
    // image layout changed). A plain byte scan has no such state.
    let bytes = LOGO.as_bytes();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let mut content = false;
        for &byte in &bytes[start..end] {
            if byte != b' ' && byte != b'\r' && byte != b'\t' {
                content = true;
                break;
            }
        }
        if content {
            let mut trimmed = end;
            while trimmed > start
                && (bytes[trimmed - 1] == b' ' || bytes[trimmed - 1] == b'\r')
            {
                trimmed -= 1;
            }
            if let Ok(line) = core::str::from_utf8(&bytes[start..trimmed]) {
                syscall::print(line);
            }
            syscall::print("\r\n");
        }
        start = end.saturating_add(1);
    }
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
