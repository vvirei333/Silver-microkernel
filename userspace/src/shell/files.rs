//! File commands: the SFS write path, directory operations and usage.
//!
//! Every command here reaches the filesystem through the SFS server, so the
//! module owns the single-shot request bounds as well.

use super::*;
use super::proc::run_utility;
use crate::syscall;

/// On-disk SFS server opcodes for the write path (`sfsinfo` uses opcode 0).
const SFS_OP_TOUCH: u8 = 1;

const SFS_OP_WRITE_FILE: u8 = 2;

const SFS_OP_READ_FILE: u8 = 3;

const SFS_NAME_DATA_LIMIT: usize = 29;

/// Reads the SFS superblock through the filesystem server.
///
/// The server is resolved from the task table on every call, so it may be
/// respawned (or absent) without the shell caching a stale task ID.
pub(super) fn sfs_info() {
    syscall::print("SFS: mounted by the kernel at /sfs\r\n");
}

pub(super) fn execute_echo(arguments: &str) {
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

pub(super) fn split_redirection(input: &str) -> Result<(&str, Option<(&str, bool)>), ()> {
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

pub(super) fn unquote(text: &str) -> &str {
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

pub(super) fn write_file(path: &str, text: &str, append: bool) {
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

pub(super) fn touch_command(path: Option<&str>) {
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

pub(super) fn make_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: mkdir <path>\r\n");
        return;
    };
    if !syscall::fs_mkdir(path) {
        syscall::print("mkdir: cannot create directory\r\n");
    }
}

pub(super) fn remove_file(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: rm <file>\r\n");
        return;
    };
    if !syscall::fs_unlink(path) {
        syscall::print("rm: cannot remove file (missing, open, or a directory)\r\n");
    }
}

pub(super) fn remove_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: rmdir <path>\r\n");
        return;
    };
    if !syscall::fs_rmdir(path) {
        syscall::print("rmdir: directory is missing, not empty, in use, or invalid\r\n");
    }
}

pub(super) fn rename_node(source: Option<&str>, destination: Option<&str>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        syscall::print("Usage: mv <old> <new>\r\n");
        return;
    };
    if !syscall::fs_rename(source, destination) {
        syscall::print("mv: cannot rename (missing, same, open, busy, or bad destination)\r\n");
    }
}

pub(super) fn replace_node(source: Option<&str>, destination: Option<&str>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        syscall::print("Usage: replace <source> <destination>\r\n");
        return;
    };
    if !syscall::fs_replace(source, destination) {
        syscall::print("replace: cannot replace (missing, open, busy, or bad path)\r\n");
    }
}

pub(super) fn change_directory(path: Option<&str>) {
    let Some(path) = path else {
        syscall::print("Usage: cd <path>\r\n");
        return;
    };
    if !syscall::fs_chdir(path) {
        syscall::print("cd: no such directory\r\n");
    }
}

pub(super) fn print_working_directory() {
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

pub(super) fn show_usage(path: Option<&str>) {
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