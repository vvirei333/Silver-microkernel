//! Command dispatch, and the commands that do not belong to a family.
//!
//! execute is the single entry point from the input loop; the family modules
//! are glob-imported because this is the one place that names them all.

use super::bench::*;
use super::files::*;
use super::net::*;
use super::proc::*;
use super::*;
use crate::syscall;

/// Names offered by Tab completion. Keep this in step with the `execute`
/// arms below; aliases are listed because they are valid first words too.
const COMMANDS: &[&str] = &[
    "help", "fastfetch", "sfsinfo", "memtest", "bench", "ping", "ip",
    "ifconfig", "dns", "wget", "wolfser", "browser", "ls", "bigdir",
    "bigfile", "cat", "touch", "run", "sl", "mkdir", "rm", "rmdir",
    "mv", "replace", "cd", "pwd", "clear", "uname", "health", "watch",
    "unwatch", "du", "ps", "spawn", "bg", "secprobe", "fg", "kill",
    "wait", "svc", "poweroff", "reboot", "grep", "wc", "head", "tail",
    "cp", "find", "seq", "basename", "dirname", "true", "false", "sleep",
    "echo", "say",
    #[cfg(feature = "linux-abi")]
    "linuxrun",
    #[cfg(feature = "linux-abi")]
    "linuxstart",
    #[cfg(feature = "linux-abi")]
    "phosh",
];

/// Command names for the input loop's Tab completion.
pub fn names() -> &'static [&'static str] {
    COMMANDS
}

pub fn execute(command: &str) {
    let command = command.trim();
    // Reap finished `spawn`/`bg` children before doing anything else, so `ps`
    // and `health` reflect live tasks instead of accumulating dead records.
    reap_unreaped();
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

    #[cfg(feature = "linux-abi")]
    if let Some(name) = command.strip_prefix("linuxrun-") {
        run_linux_alias(name);
        return;
    }

    let mut words = command.split_whitespace();
    match words.next() {
        None => {}
        Some("help") => {
            syscall::print("howl commands:\r\n");
            syscall::print("  Tab        Complete a command name; Up/Down recall history\r\n");
            syscall::print("  Ctrl-A/E   Move to the start/end of the line\r\n");
            syscall::print("  Ctrl-U/K/W Erase before, after, or one word before the cursor\r\n");
            syscall::print("  help       Show this command list\r\n");
            syscall::print("  fastfetch  Show the aligned system summary\r\n");
            syscall::print("  sfsinfo    Show the mounted SFS superblock\r\n");
            syscall::print("  memtest    mmap 1 MiB, write/read it back\r\n");
            syscall::print("  bench [syscall|sched|fs|vm|memcpy|all]\r\n");
            syscall::print("             Cycle-accurate fast-path timings (ns/op)\r\n");
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
            {
                syscall::print("  linuxrun <path>  Run a Linux ABI ELF (x8 = Linux syscall)\r\n");
                syscall::print("  linuxrun-<app>   Run /sfs/apps/<app> or /sfs/<app>\r\n");
                syscall::print("  linuxstart <app> Run /sfs/<app>-serve, then /sfs/<app>\r\n");
                syscall::print("  phosh            Shortcut for `linuxstart phosh`\r\n");
            }
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
            syscall::print("  watch <addr> [len]  Arm a hardware write watchpoint (gdb-stub)\r\n");
            syscall::print("  unwatch    Disarm the watchpoint\r\n");
            syscall::print("  du [path]  SFS inode/block usage of a subtree\r\n");
            syscall::print("  ps         List tasks and their states\r\n");
            syscall::print("  spawn <program> [arg|text]\r\n");
            syscall::print("             Start a task and hand it the console\r\n");
            syscall::print("  bg <program> [arg|text]\r\n");
            syscall::print("             Start a task and keep the console\r\n");
            syscall::print("  secprobe <grant_foreign|dma_cross_read|mailbox_quota|dma_region_confine|irq_orphan>\r\n");
            syscall::print("             Native M2 capability/IPC regression (security suite)\r\n");
            syscall::print("  fg <cap>   Resume a task in the foreground\r\n");
            syscall::print("  Ctrl-Z     Stop the foreground task and return to shell\r\n");
            syscall::print("  kill <cap> Kill a task\r\n");
            syscall::print("  wait <cap> Wait for a task and print its exit status\r\n");
            syscall::print("  svc restart <name>\r\n");
            syscall::print("             Restart a supervised service (scout, net_driver,\r\n");
            syscall::print("             net_server, howl, howler); it also auto-restarts\r\n");
            syscall::print("  reboot     Reset the machine through PSCI\r\n");
            syscall::print("  grep <pattern> <file> | wc <file> | head/tail [n] <file>\r\n");
            syscall::print("  cp <src> <dst> | find [path] | seq N | basename/dirname <path>\r\n");
            syscall::print("  true | false | sleep <seconds>\r\n");
        }
        Some("fastfetch") => fastfetch(),
        Some("sfsinfo") => sfs_info(),
        Some("memtest") => memtest(),
        Some("bench") => bench_command(words.next()),
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
        #[cfg(feature = "linux-abi")]
        Some("linuxstart") => run_linux_start(words.next().unwrap_or("")),
        #[cfg(feature = "linux-abi")]
        Some("phosh") => run_linux_start("phosh"),
        Some("sl") => run_utility(syscall::PROGRAM_SL, ""),
        Some("mkdir") => make_directory(words.next()),
        Some("rm") => remove_file(words.next()),
        Some("rmdir") => remove_directory(words.next()),
        Some("mv") => rename_node(words.next(), words.next()),
        Some("replace") => replace_node(words.next(), words.next()),
        Some("cd") => change_directory(words.next()),
        Some("pwd") => print_working_directory(),
        Some("clear") => clear_screen(),
        Some("uname") => syscall::print(concat!(
            "Silver Kernel v",
            env!("SILVER_VERSION"),
            " aarch64 (WerewolfOS)\r\n"
        )),
        Some("health") => show_health(),
        Some("watch") => watch_command(words.next(), words.next()),
        Some("unwatch") => {
            let _ = syscall::debug_watch(0, 0);
            syscall::print("watch: off\r\n");
        }
        Some("du") => show_usage(words.next()),
        Some("ps") => show_tasks(),
        Some("spawn") => spawn_program(words.next(), words.next(), false),
        Some("bg") => spawn_program(words.next(), words.next(), true),
        Some("secprobe") => secprobe_command(words.next()),
        Some("fg") => foreground_task(words.next()),
        Some("kill") => kill_task(words.next()),
        Some("wait") => wait_task(words.next()),
        Some("svc") => svc_command(words.next(), words.next()),
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

pub(super) fn print_offset(offset: usize) {
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

pub(super) fn print_hex_byte(b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let buf = [HEX[(b >> 4) as usize], HEX[(b & 0xF) as usize]];
    if let Ok(s) = core::str::from_utf8(&buf) {
        syscall::print(s);
    }
}

pub(super) fn parse_hex(text: &str) -> Option<u64> {
    let text = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    if text.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for byte in text.bytes() {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit as u64)?;
    }
    Some(value)
}

pub(super) fn watch_command(address: Option<&str>, length: Option<&str>) {
    let Some(text) = address else {
        syscall::print("Usage: watch <address> [length]\r\n");
        return;
    };
    let Some(address) = parse_hex(text) else {
        syscall::print("watch: bad address\r\n");
        return;
    };
    let length = length.and_then(parse_hex).unwrap_or(8);
    if syscall::debug_watch(address, length) {
        syscall::print("watch: armed at 0x");
        syscall::print_hex_u64(address);
        syscall::print(" len=");
        syscall::print_dec_u64(length as u64);
        syscall::print("\r\n");
    } else {
        syscall::print("watch: failed (debug support or range)\r\n");
    }
}

pub fn fastfetch() {
    let uptime = syscall::uptime_ticks();
    let memory = syscall::memory_info();
    let health = syscall::health_info();
    let disk = syscall::fs_usage("/sfs");

    print_logo();
    syscall::print("\r\n");
    plain("werewolf@howl");
    plain("-------------");
    field("OS:", concat!("WerewolfOS ", env!("SILVER_VERSION")));
    field("Kernel:", concat!("Silver Microkernel ", env!("SILVER_VERSION")));
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

pub(super) fn plain(text: &str) {
    syscall::print(text);
    syscall::print("\r\n");
}

pub(super) fn heading(label: &str) {
    syscall::print(label);
    let mut column = label.len();
    while column < FIELD_WIDTH {
        syscall::print(" ");
        column += 1;
    }
}

pub(super) fn field(label: &str, value: &str) {
    heading(label);
    syscall::print(value);
    syscall::print("\r\n");
}

/// A 16-colour strip, like the one at the bottom of fastfetch. Two spaces per
/// block keeps it readable on the 80-column serial console.
pub(super) fn print_color_bar() {
    syscall::print("\r\n");
    syscall::print(
        "\x1b[40m  \x1b[41m  \x1b[42m  \x1b[43m  \x1b[44m  \x1b[45m  \x1b[46m  \x1b[47m  \x1b[0m\r\n",
    );
    syscall::print(
        "\x1b[100m  \x1b[101m  \x1b[102m  \x1b[103m  \x1b[104m  \x1b[105m  \x1b[106m  \x1b[107m  \x1b[0m\r\n",
    );
}

pub(super) fn print_percent(used: u64, total: u64) {
    let percent = if total == 0 {
        0
    } else {
        used.saturating_mul(100) / total
    };
    syscall::print_dec_u64(percent);
    syscall::print("%");
}

pub(super) fn print_size(bytes: u64) {
    if bytes >= MIB {
        print_mib(bytes);
    } else {
        syscall::print_dec_u64(bytes / 1024);
        syscall::print(" KiB");
    }
}

pub(super) fn print_logo() {
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

pub(super) fn print_uptime(ticks: u64) {
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

pub(super) fn print_mib(bytes: u64) {
    let tenths = bytes.saturating_mul(10) / MIB;
    syscall::print_dec_u64(tenths / 10);
    syscall::print(".");
    syscall::print_dec_u64(tenths % 10);
    syscall::print(" MiB");
}
