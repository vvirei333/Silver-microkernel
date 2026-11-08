#![no_std]
#![no_main]

//! Small multi-call Unix toolbox. One ELF, many commands:
//! `grep`, `wc`, `head`, `tail`, `cp`, `find`, `seq`, `basename`,
//! `dirname`, `true`, `false`, `sleep`.
//!
//! Arguments arrive as one space-separated string through `SYS_ARGS` (the
//! console shell's `run_utility` contract), not as a full argv. Quoting and
//! pipelines are out of scope for this first layer.

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const PATH_MAX: usize = 191;
const LINE_MAX: usize = 512;
const DIR_CAPACITY: usize = 48;
const TAIL_MAX: usize = 16 * 1024;

const EMPTY_ENTRY: syscall::DirEntry = syscall::DirEntry {
    name: [0; syscall::DIR_NAME_CAPACITY],
    name_len: 0,
    kind: 0,
};

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut arguments = [0_u8; 192];
    let len = syscall::arguments(&mut arguments).unwrap_or(0);
    let line = core::str::from_utf8(&arguments[..len]).unwrap_or("").trim();
    let mut words = line.split_whitespace();
    let Some(tool) = words.next() else {
        usage();
    };
    match tool {
        "grep" => grep(words.next(), words.next()),
        "wc" => wc(words.next()),
        "head" => head(words.next(), words.next()),
        "tail" => tail(words.next(), words.next()),
        "cp" => copy(words.next(), words.next()),
        "find" => find(words.next().unwrap_or("."), 0),
        "seq" => seq(words.next(), words.next()),
        "basename" => basename(words.next()),
        "dirname" => dirname(words.next()),
        "sleep" => sleep_cmd(words.next()),
        "true" => syscall::exit_with_status(0),
        "false" => syscall::exit_with_status(1),
        _ => {
            syscall::print("utils: unknown command '");
            syscall::print(tool);
            syscall::print("'\r\n");
            usage();
        }
    }
    syscall::exit()
}

fn usage() -> ! {
    syscall::print(
        "usage: <tool> ...\r\n\
         tools: grep PATTERN FILE | wc FILE | head [N] FILE | tail [N] FILE |\r\n\
         cp SRC DST | find [PATH] | seq N | seq START END |\r\n\
         basename PATH | dirname PATH | true | false | sleep SECONDS\r\n",
    );
    syscall::exit_with_status(1)
}

fn print_line(bytes: &[u8]) {
    syscall::print_bytes(bytes);
    syscall::print("\r\n");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn grep(pattern: Option<&str>, path: Option<&str>) {
    let (Some(pattern), Some(path)) = (pattern, path) else {
        syscall::print("utils: grep PATTERN FILE\r\n");
        syscall::exit_with_status(2);
    };
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_READ) else {
        syscall::print("grep: cannot open file\r\n");
        syscall::exit_with_status(2);
    };
    let mut chunk = [0_u8; 256];
    let mut line = [0_u8; LINE_MAX];
    let mut line_len = 0usize;
    let mut matched = 0u64;
    loop {
        match syscall::fs_read(fd, &mut chunk) {
            Some(0) => break,
            Some(count) => {
                for &byte in &chunk[..count] {
                    if byte == b'\n' {
                        if contains(&line[..line_len], pattern.as_bytes()) {
                            print_line(&line[..line_len]);
                            matched += 1;
                        }
                        line_len = 0;
                    } else if line_len < line.len() - 1 {
                        line[line_len] = byte;
                        line_len += 1;
                    }
                }
            }
            None => {
                let _ = syscall::fs_close(fd);
                syscall::print("grep: read failed\r\n");
                syscall::exit_with_status(2);
            }
        }
    }
    if line_len > 0 && contains(&line[..line_len], pattern.as_bytes()) {
        print_line(&line[..line_len]);
        matched += 1;
    }
    let _ = syscall::fs_close(fd);
    syscall::exit_with_status(if matched == 0 { 1 } else { 0 });
}

fn wc(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("utils: wc FILE\r\n");
        syscall::exit_with_status(2);
    };
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_READ) else {
        syscall::print("wc: cannot open file\r\n");
        syscall::exit_with_status(2);
    };
    let mut chunk = [0_u8; 256];
    let (mut bytes, mut lines, mut words) = (0u64, 0u64, 0u64);
    let mut in_word = false;
    loop {
        match syscall::fs_read(fd, &mut chunk) {
            Some(0) => break,
            Some(count) => {
                bytes += count as u64;
                for &byte in &chunk[..count] {
                    if byte == b'\n' {
                        lines += 1;
                    }
                    let space = byte == b' ' || byte == b'\t' || byte == b'\r' || byte == b'\n';
                    if space {
                        in_word = false;
                    } else if !in_word {
                        in_word = true;
                        words += 1;
                    }
                }
            }
            None => {
                let _ = syscall::fs_close(fd);
                syscall::print("wc: read failed\r\n");
                syscall::exit_with_status(2);
            }
        }
    }
    let _ = syscall::fs_close(fd);
    syscall::print_dec_u64(lines);
    syscall::print(" ");
    syscall::print_dec_u64(words);
    syscall::print(" ");
    syscall::print_dec_u64(bytes);
    syscall::print(" ");
    print_line(path.as_bytes());
}

fn head(count: Option<&str>, path: Option<&str>) {
    let (count, path) = match (count, path) {
        (Some(first), Some(path)) => (parse_u64(first).unwrap_or(10), path),
        (Some(path), None) => (10, path),
        _ => {
            syscall::print("utils: head [N] FILE\r\n");
            syscall::exit_with_status(2);
        }
    };
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_READ) else {
        syscall::print("head: cannot open file\r\n");
        syscall::exit_with_status(2);
    };
    let mut chunk = [0_u8; 256];
    let mut printed = 0u64;
    let mut stop = false;
    'read: while !stop {
        match syscall::fs_read(fd, &mut chunk) {
            Some(0) => break,
            Some(read) => {
                for (index, &byte) in chunk[..read].iter().enumerate() {
                    syscall::print_bytes(&[byte]);
                    if byte == b'\n' {
                        printed += 1;
                        if printed >= count {
                            let _ = index;
                            stop = true;
                            break 'read;
                        }
                    }
                }
            }
            None => break,
        }
    }
    let _ = syscall::fs_close(fd);
}

fn tail(count: Option<&str>, path: Option<&str>) {
    let (count, path) = match (count, path) {
        (Some(first), Some(path)) => (parse_u64(first).unwrap_or(10) as usize, path),
        (Some(path), None) => (10, path),
        _ => {
            syscall::print("utils: tail [N] FILE\r\n");
            syscall::exit_with_status(2);
        }
    };
    let Some(fd) = syscall::fs_open(path, syscall::OPEN_READ) else {
        syscall::print("tail: cannot open file\r\n");
        syscall::exit_with_status(2);
    };
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(TAIL_BUFFER) };
    let mut length = 0usize;
    loop {
        if length == buffer.len() {
            break;
        }
        match syscall::fs_read(fd, &mut buffer[length..]) {
            Some(0) => break,
            Some(read) => length += read,
            None => break,
        }
    }
    let _ = syscall::fs_close(fd);
    if length == 0 {
        return;
    }
    let limit = count.max(1);
    let mut seen = 0usize;
    let mut start = length;
    while start > 0 && seen < limit {
        start -= 1;
        if buffer[start] == b'\n' && start + 1 < length {
            seen += 1;
        }
    }
    if start > 0 && buffer[start] == b'\n' {
        start += 1;
    }
    syscall::print_bytes(&buffer[start..length]);
    if buffer[length - 1] != b'\n' {
        syscall::print("\r\n");
    }
}

static mut TAIL_BUFFER: [u8; TAIL_MAX] = [0; TAIL_MAX];

fn copy(source: Option<&str>, destination: Option<&str>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        syscall::print("utils: cp SRC DST\r\n");
        syscall::exit_with_status(2);
    };
    let Some(input) = syscall::fs_open(source, syscall::OPEN_READ) else {
        syscall::print("cp: cannot open source\r\n");
        syscall::exit_with_status(2);
    };
    let _ = syscall::fs_create(destination);
    let Some(output) = syscall::fs_open(destination, syscall::OPEN_WRITE) else {
        let _ = syscall::fs_close(input);
        syscall::print("cp: cannot open destination\r\n");
        syscall::exit_with_status(2);
    };
    let mut chunk = [0_u8; 512];
    loop {
        match syscall::fs_read(input, &mut chunk) {
            Some(0) => break,
            Some(read) => {
                if syscall::fs_write(output, &chunk[..read]).is_none() {
                    let _ = syscall::fs_close(input);
                    let _ = syscall::fs_close(output);
                    syscall::print("cp: write failed\r\n");
                    syscall::exit_with_status(2);
                }
            }
            None => {
                let _ = syscall::fs_close(input);
                let _ = syscall::fs_close(output);
                syscall::print("cp: read failed\r\n");
                syscall::exit_with_status(2);
            }
        }
    }
    let _ = syscall::fs_close(input);
    let _ = syscall::fs_close(output);
    syscall::exit_with_status(0);
}

fn find(path: &str, depth: u32) {
    if path.len() > PATH_MAX {
        return;
    }
    print_line(path.as_bytes());
    if depth >= 4 {
        return;
    }
    let mut entries = [EMPTY_ENTRY; DIR_CAPACITY];
    let Some(count) = syscall::fs_read_dir(path, &mut entries) else {
        return;
    };
    for entry in &entries[..count.min(DIR_CAPACITY)] {
        let Some(name) = entry.name() else {
            continue;
        };
        let mut child = [0_u8; PATH_MAX + 1];
        let mut length = 0usize;
        for &byte in path.as_bytes() {
            if length == PATH_MAX {
                break;
            }
            child[length] = byte;
            length += 1;
        }
        if length > 0 && child[length - 1] != b'/' {
            if length == PATH_MAX {
                continue;
            }
            child[length] = b'/';
            length += 1;
        }
        for &byte in name.as_bytes() {
            if length == PATH_MAX {
                break;
            }
            child[length] = byte;
            length += 1;
        }
        let Ok(child) = core::str::from_utf8(&child[..length]) else {
            continue;
        };
        if entry.is_directory() {
            find(child, depth + 1);
        } else {
            print_line(child.as_bytes());
        }
    }
}

fn seq(first: Option<&str>, second: Option<&str>) {
    let (start, end) = match (first, second) {
        (Some(end), None) => (Some(1), parse_u64(end)),
        (Some(start), Some(end)) => (parse_u64(start), parse_u64(end)),
        _ => {
            syscall::print("utils: seq N | seq START END\r\n");
            syscall::exit_with_status(2);
        }
    };
    let (Some(start), Some(end)) = (start, end) else {
        syscall::print("seq: integer arguments required\r\n");
        syscall::exit_with_status(2);
    };
    if end < start {
        return;
    }
    // Bound the output so a typo cannot flood the console forever.
    let count = (end - start).min(100_000);
    let mut value = start;
    let last = start.saturating_add(count);
    while value <= last && value <= end {
        syscall::print_dec_u64(value);
        syscall::print("\r\n");
        if value == u64::MAX {
            break;
        }
        value += 1;
    }
}

fn basename(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("utils: basename PATH\r\n");
        syscall::exit_with_status(2);
    };
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        print_line(b"/");
    } else {
        print_line(name.as_bytes());
    }
}

fn dirname(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("utils: dirname PATH\r\n");
        syscall::exit_with_status(2);
    };
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => print_line(b"/"),
        Some(index) => print_line(&trimmed.as_bytes()[..index]),
        None => print_line(b"."),
    }
}

fn sleep_cmd(seconds: Option<&str>) {
    let Some(seconds) = seconds.and_then(parse_u64) else {
        syscall::print("utils: sleep SECONDS\r\n");
        syscall::exit_with_status(2);
    };
    syscall::sleep(seconds.saturating_mul(1000));
}

fn parse_u64(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
