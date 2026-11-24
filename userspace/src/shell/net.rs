//! Network commands: ping, ip, dns, wget and the Wolfser window.
//!
//! Requests go to net_server over the fixed net_ipc protocol; task IDs are
//! resolved from the live table on every call.

use super::*;
use super::proc::find_task_id;
use crate::{ipc, net_ipc, syscall};

/// Sends one ICMP echo through the network server.
///
/// The server owns smoltcp and the driver, so the shell only ships the parsed
/// address and waits for the outcome. The server answers "timeout" after its
/// own two-second wait, so the shell's deadline must exceed that: three
/// seconds leaves a full second for scheduler latency on the reply.
pub(super) fn ping_command(argument: Option<&str>) {
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
pub(super) fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
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

pub(super) fn print_dotted(address: [u8; 4]) {
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
pub(super) fn net_wait_reply(server: u64, opcode: u8, timeout_ticks: u64) -> Option<ipc::Message> {
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
pub(super) fn net_window(server: u64) -> Option<u64> {
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
pub(super) fn write_window_request(base: u64, text: &str) -> bool {
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

pub(super) fn net_error_text(code: u8) -> &'static str {
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
pub(super) fn ip_command() {
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
pub(super) fn dns_command(name: Option<&str>) {
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
pub(super) fn wolfser_command(url: Option<&str>) {
    if find_task_id(syscall::PROGRAM_WOLFSER).is_some() {
        syscall::print("wolfser: already running in the host window\r\n");
        return;
    }
    let url = url.unwrap_or("");
    let Some(cap_id) = syscall::spawn_with_args(syscall::PROGRAM_WOLFSER, 0, url) else {
        syscall::print("wolfser: cannot start the program\r\n");
        return;
    };
    // Wolfser maps a per-client window from net_server; the server grants
    // exactly that range when it handles the first OP_SHARE, so no
    // owner-wide share right is delegated here.
    syscall::print("wolfser: started (host viewer shows the window)\r\n");
    let _ = cap_id;
}

pub(super) fn wget_command(url: Option<&str>, destination: Option<&str>) {
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
