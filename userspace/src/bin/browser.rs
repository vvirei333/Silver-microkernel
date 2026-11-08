//! WerewolfOS browser: fetch a page and draw it into the shared window.
//!
//! The program owns no compositor: it writes one RGBA surface into an
//! ivshmem BAR that the host viewer maps from the same file, and publishes
//! frames through the header's `frame_seq`. HTML is reduced to readable text
//! (scripts, styles and tags stripped, entities decoded, block tags become
//! line breaks); this is deliberately a small first renderer, not a layout
//! engine.

#![no_std]
#![no_main]

#[path = "../font5x7.rs"]
mod font5x7;
#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../window_proto.rs"]
mod window;

use core::panic::PanicInfo;

const WIDTH: u32 = 900;
const HEIGHT: u32 = 640;
const STRIDE: u32 = WIDTH * 4;
const HEADER_HEIGHT: u32 = 44;
const FOOTER_HEIGHT: u32 = 14;
const SCALE: u32 = 2;
const CELL_WIDTH: u32 = 12; // (5 + 1) * scale
const CELL_HEIGHT: u32 = 16; // (7 + 1) * scale
const LINE_HEIGHT: u32 = 18;
const MARGIN: u32 = 12;
const TEXT_COLUMNS: u32 = (WIDTH - 2 * MARGIN) / CELL_WIDTH;
const BODY_ROWS: u32 = (HEIGHT - HEADER_HEIGHT - FOOTER_HEIGHT - MARGIN) / LINE_HEIGHT;

const COLOR_BACKGROUND: u32 = 0xFFFF_FFFF;
const COLOR_HEADER: u32 = 0xFF5F_3A1E;
const COLOR_HEADER_TEXT: u32 = 0xFFFF_FFFF;
const COLOR_TEXT: u32 = 0xFF20_2020;
const COLOR_MUTED: u32 = 0xFF60_6060;

const DEFAULT_URL: &str = "http://example.com/";
const IVSHMEM_VENDOR: u16 = 0x1AF4;
const IVSHMEM_DEVICE: u16 = 0x1110;
const IVSHMEM_BAR: u64 = 0x1040_0000;
const WINDOW_MAX_SIZE: u64 = 8 * 1024 * 1024;

const TEXT_CAPACITY: usize = 24 * 1024;
const TITLE_CAPACITY: usize = 96;
const URL_CAPACITY: usize = 160;

const TICKS_PER_SECOND: u64 = 100;
const FETCH_TIMEOUT_TICKS: u64 = 30 * TICKS_PER_SECOND;
const RESPONSE_POLL_TICKS: u64 = 10;

const KEY_UP: u16 = 1;
const KEY_DOWN: u16 = 2;
const KEY_PAGE_UP: u16 = 3;
const KEY_PAGE_DOWN: u16 = 4;
const KEY_HOME: u16 = 5;
const KEY_END: u16 = 6;

static mut TEXT: [u8; TEXT_CAPACITY] = [0; TEXT_CAPACITY];
static mut TEXT_LEN: usize = 0;
static mut TITLE: [u8; TITLE_CAPACITY] = [0; TITLE_CAPACITY];
static mut TITLE_LEN: usize = 0;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut arguments = [0_u8; 192];
    let length = syscall::arguments(&mut arguments).unwrap_or(0);
    let url = core::str::from_utf8(&arguments[..length])
        .unwrap_or("")
        .trim();
    let url = if url.is_empty() { DEFAULT_URL } else { url };

    let Some(server) = find_task_id(syscall::PROGRAM_NET_SERVER) else {
        fatal(11, "NETWORK SERVER IS NOT RUNNING");
    };
    let window_base = match open_window() {
        Ok(base) => base,
        Err(stage) => fatal(20 + stage, "NO IVSHMEM WINDOW (START QEMU WITH -device ivshmem-plain)"),
    };
    let Some(net_base) = map_net_window(server) else {
        fatal(13, "CANNOT MAP THE NETWORK WINDOW");
    };

    let mut state = PageState {
        status: 0,
        body_len: 0,
        error: "",
    };
    load(server, net_base, url, &mut state);
    let mut scroll = 0usize;
    paint(window_base, url, &state, scroll);
    let mut dirty = false;

    loop {
        let head = window::read_u32(window_base, window::FIELD_INPUT_HEAD);
        let mut tail = window::read_u32(window_base, window::FIELD_INPUT_TAIL);
        while tail != head {
            let event = window::read_input(window_base, tail);
            tail = (tail + 1) % window::INPUT_SLOTS;
            match event.kind {
                window::INPUT_KEY => {
                    let key = event.key;
                    if key == u16::from(b'Q') || key == u16::from(b'q') {
                        syscall::exit();
                    } else if key == u16::from(b'R') || key == u16::from(b'r') {
                        load(server, net_base, url, &mut state);
                        scroll = 0;
                        dirty = true;
                    } else if key == KEY_DOWN {
                        scroll = scroll.saturating_add(1);
                        dirty = true;
                    } else if key == KEY_UP {
                        scroll = scroll.saturating_sub(1);
                        dirty = true;
                    } else if key == KEY_PAGE_DOWN {
                        scroll = scroll.saturating_add(BODY_ROWS as usize);
                        dirty = true;
                    } else if key == KEY_PAGE_UP {
                        scroll = scroll.saturating_sub(BODY_ROWS as usize);
                        dirty = true;
                    } else if key == KEY_HOME {
                        scroll = 0;
                        dirty = true;
                    } else if key == KEY_END {
                        scroll = line_count() as usize;
                        dirty = true;
                    }
                }
                window::INPUT_SCROLL => {
                    let delta = event.value as i8;
                    if delta >= 0 {
                        scroll = scroll.saturating_add(delta as usize);
                    } else {
                        scroll = scroll.saturating_sub((-delta) as usize);
                    }
                    dirty = true;
                }
                _ => {}
            }
        }
        if tail != window::read_u32(window_base, window::FIELD_INPUT_TAIL) {
            window::write_u32(window_base, window::FIELD_INPUT_TAIL, tail);
        }

        if dirty {
            let lines = line_count() as usize;
            let max_scroll = lines.saturating_sub(BODY_ROWS as usize);
            if scroll > max_scroll {
                scroll = max_scroll;
            }
            paint(window_base, url, &state, scroll);
            dirty = false;
        }
        syscall::sleep(50);
    }
}

struct PageState {
    status: u16,
    body_len: usize,
    error: &'static str,
}

fn load(server: u64, net_base: u64, url: &str, state: &mut PageState) {
    state.status = 0;
    state.body_len = 0;
    state.error = "";
    if !write_request(net_base, url) {
        state.error = "URL IS TOO LONG";
        return;
    }
    if !syscall::send(server, &net_ipc::http_request()) {
        state.error = "NETWORK SERVER IS NOT RESPONDING";
        return;
    }
    let Some(reply) = wait_reply(server, net_ipc::OP_HTTP_REPLY, FETCH_TIMEOUT_TICKS) else {
        state.error = "NO REPLY FROM THE NETWORK SERVER";
        return;
    };
    let Some((code, status, body_len)) = net_ipc::http_fields(&reply) else {
        state.error = "MALFORMED NETWORK REPLY";
        return;
    };
    if code != net_ipc::NET_OK {
        state.error = match code {
            net_ipc::NET_ERR_DNS => "NAME RESOLUTION FAILED",
            net_ipc::NET_ERR_CONNECT => "CONNECTION FAILED",
            net_ipc::NET_ERR_TIMEOUT => "REQUEST TIMED OUT",
            net_ipc::NET_ERR_URL => "INVALID URL",
            net_ipc::NET_ERR_TLS => "HTTPS IS NOT SUPPORTED YET (USE HTTP://)",
            net_ipc::NET_ERR_RESPONSE => "MALFORMED HTTP RESPONSE",
            net_ipc::NET_ERR_BUSY => "NETWORK SERVER BUSY",
            _ => "NETWORK ERROR",
        };
        return;
    }
    state.status = status;
    state.body_len = body_len as usize;
    extract(net_base, body_len as usize);
}

fn extract(net_base: u64, body_len: usize) {
    let body = unsafe {
        core::slice::from_raw_parts(
            (net_base + net_ipc::CLIENT_BODY) as *const u8,
            body_len.min(net_ipc::CLIENT_BODY_MAX),
        )
    };
    let text = unsafe { &mut *core::ptr::addr_of_mut!(TEXT) };
    let title = unsafe { &mut *core::ptr::addr_of_mut!(TITLE) };
    let mut text_len = 0usize;
    let mut title_len = 0usize;
    let mut index = 0usize;
    let mut space_pending = false;
    let mut newline_pending = false;
    while index < body.len() && text_len + 2 < text.len() {
        let byte = body[index];
        if byte == b'<' {
            let (tag, after) = read_tag(body, index);
            index = after;
            if tag_eq(tag, b"script") || tag_eq(tag, b"style") {
                index = skip_element(body, index, tag);
                newline_pending = true;
                continue;
            }
            if tag_eq(tag, b"title") {
                let (value, after_title) = read_text_until(body, index, b"title");
                for &character in value {
                    if title_len < title.len() - 1 {
                        title[title_len] = character;
                        title_len += 1;
                    }
                }
                index = after_title;
                continue;
            }
            if is_block_tag(tag) {
                newline_pending = true;
            }
            continue;
        }
        if byte == b'&' {
            if let Some((character, after)) = decode_entity(body, index) {
                push_char(
                    text,
                    &mut text_len,
                    character,
                    &mut space_pending,
                    &mut newline_pending,
                );
                index = after;
                continue;
            }
        }
        if byte == b'\n' || byte == b'\r' {
            newline_pending = true;
            index += 1;
            continue;
        }
        if byte == b' ' || byte == b'\t' {
            if text_len > 0 {
                space_pending = true;
            }
            index += 1;
            continue;
        }
        let byte = if (0x20..0x7F).contains(&byte) { byte } else { b'?' };
        push_char(
            text,
            &mut text_len,
            byte,
            &mut space_pending,
            &mut newline_pending,
        );
        index += 1;
    }
    unsafe {
        *core::ptr::addr_of_mut!(TEXT_LEN) = text_len;
        *core::ptr::addr_of_mut!(TITLE_LEN) = title_len;
    }
}

fn push_char(
    text: &mut [u8],
    length: &mut usize,
    byte: u8,
    space_pending: &mut bool,
    newline_pending: &mut bool,
) {
    if *newline_pending {
        if *length > 0 && text[*length - 1] != b'\n' {
            text[*length] = b'\n';
            *length += 1;
        }
        *newline_pending = false;
        *space_pending = false;
    } else if *space_pending && *length > 0 {
        text[*length] = b' ';
        *length += 1;
        *space_pending = false;
    }
    text[*length] = byte;
    *length += 1;
}

fn read_tag<'a>(body: &'a [u8], start: usize) -> (&'a [u8], usize) {
    let mut index = start + 1;
    // Skip a closing slash and leading spaces.
    while index < body.len() && body[index] == b' ' {
        index += 1;
    }
    let name_start = if index < body.len() && body[index] == b'/' {
        index + 1
    } else {
        index
    };
    index = name_start;
    while index < body.len() && (body[index].is_ascii_alphanumeric() || body[index] == b'!') {
        index += 1;
    }
    let name = &body[name_start..index];
    let mut after = index;
    let mut quote = 0u8;
    while after < body.len() {
        let byte = body[after];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            after += 1;
            break;
        }
        after += 1;
    }
    (name, after)
}

fn tag_eq(tag: &[u8], name: &[u8]) -> bool {
    tag.len() == name.len() && tag.eq_ignore_ascii_case(name)
}

fn is_block_tag(tag: &[u8]) -> bool {
    for name in [
        &b"br"[..],
        &b"p"[..],
        &b"div"[..],
        &b"li"[..],
        &b"tr"[..],
        &b"h1"[..],
        &b"h2"[..],
        &b"h3"[..],
        &b"h4"[..],
        &b"h5"[..],
        &b"h6"[..],
        &b"title"[..],
        &b"section"[..],
        &b"article"[..],
        &b"header"[..],
        &b"footer"[..],
        &b"nav"[..],
        &b"table"[..],
        &b"pre"[..],
        &b"ul"[..],
        &b"ol"[..],
        &b"blockquote"[..],
        &b"hr"[..],
    ] {
        if tag_eq(tag, name) {
            return true;
        }
    }
    false
}

/// Skips the body of a `<script>`/`<style>` element, returning the index after
/// its closing tag.
fn skip_element(body: &[u8], start: usize, tag: &[u8]) -> usize {
    let mut index = start;
    while index < body.len() {
        if body[index] == b'<' {
            let (name, after) = read_tag(body, index);
            if tag_eq(name, tag) {
                // `read_tag` returns `script` for both `<script>` and
                // `</script>`; the closing tag is the one whose name is
                // followed by `>`, and the opening one was already consumed.
                if body.get(index + 1) == Some(&b'/') {
                    return after;
                }
            }
            index = after;
        } else {
            index += 1;
        }
    }
    body.len()
}

fn read_text_until<'a>(body: &'a [u8], start: usize, tag: &[u8]) -> (&'a [u8], usize) {
    let mut index = start;
    while index < body.len() {
        if body[index] == b'<' {
            let (name, after) = read_tag(body, index);
            if tag_eq(name, tag) && body.get(index + 1) == Some(&b'/') {
                return (&body[start..index], after);
            }
            index = after;
        } else {
            index += 1;
        }
    }
    (&body[start..body.len().min(start + 80)], body.len())
}

fn decode_entity(body: &[u8], start: usize) -> Option<(u8, usize)> {
    let end = body[start + 1..].iter().position(|&byte| byte == b';')? + start + 1;
    let name = &body[start + 1..end];
    let character = match name {
        b"amp" => b'&',
        b"lt" => b'<',
        b"gt" => b'>',
        b"quot" => b'"',
        b"apos" | b"#39" => b'\'',
        b"nbsp" => b' ',
        _ => return None,
    };
    Some((character, end + 1))
}

fn line_count() -> u32 {
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let len = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let mut lines = 1u32;
    let mut columns = 0u32;
    for &byte in &text[..len] {
        if byte == b'\n' {
            lines += 1;
            columns = 0;
        } else {
            columns += 1;
            if columns > TEXT_COLUMNS {
                lines += 1;
                columns = 1;
            }
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Rendering

fn paint(base: u64, url: &str, state: &PageState, scroll: usize) {
    fill(base, 0, 0, WIDTH, HEIGHT, COLOR_BACKGROUND);
    fill(base, 0, 0, WIDTH, HEADER_HEIGHT, COLOR_HEADER);
    let title = unsafe {
        let length = core::ptr::read(core::ptr::addr_of!(TITLE_LEN));
        let bytes = core::slice::from_raw_parts(
            core::ptr::addr_of!(TITLE) as *const u8,
            length,
        );
        core::str::from_utf8(bytes).unwrap_or("")
    };
    let heading = if title.is_empty() {
        "WEREWOLFOS BROWSER"
    } else {
        title
    };
    draw_text(base, 12, 6, 1, heading, COLOR_HEADER_TEXT);
    draw_text(base, 12, 22, 1, url, COLOR_HEADER_TEXT);
    draw_text(
        base,
        12,
        HEIGHT - 12,
        1,
        "Q QUIT   R RELOAD   ARROWS / WHEEL SCROLL",
        COLOR_MUTED,
    );

    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    if length == 0 || !state.error.is_empty() {
        let message = if state.error.is_empty() {
            "EMPTY PAGE"
        } else {
            state.error
        };
        draw_text(base, 24, HEADER_HEIGHT + 28, 2, message, COLOR_TEXT);
        publish(base, window::STATE_ERROR);
        return;
    }

    let mut y = HEADER_HEIGHT + 6;
    let mut line = 0usize;
    let mut columns = 0u32;
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < length {
        let byte = text[index];
        if byte == b'\n' {
            if line >= scroll && line < scroll + BODY_ROWS as usize {
                draw_text_bytes(base, 12, y, 2, &text[line_start..index], COLOR_TEXT);
                y += LINE_HEIGHT;
            }
            line += 1;
            index += 1;
            line_start = index;
            columns = 0;
            continue;
        }
        columns += 1;
        if columns > TEXT_COLUMNS {
            if line >= scroll && line < scroll + BODY_ROWS as usize {
                draw_text_bytes(base, 12, y, 2, &text[line_start..index], COLOR_TEXT);
                y += LINE_HEIGHT;
            }
            line += 1;
            line_start = index;
            columns = 1;
        }
        index += 1;
    }
    if line_start < length && line >= scroll && line < scroll + BODY_ROWS as usize {
        draw_text_bytes(base, 12, y, 2, &text[line_start..length], COLOR_TEXT);
    }
    publish(base, window::STATE_READY);
}

fn publish(base: u64, state: u32) {
    window::write_u32(base, window::FIELD_STATE, state);
    let sequence = window::read_u32(base, window::FIELD_FRAME_SEQ).wrapping_add(1);
    window::write_u32(base, window::FIELD_FRAME_SEQ, sequence);
    net_ipc::fence();
}

fn fill(base: u64, x: u32, y: u32, width: u32, height: u32, color: u32) {
    let mut row = 0u32;
    while row < height && y + row < HEIGHT {
        let mut column = 0u32;
        while column < width && x + column < WIDTH {
            put_pixel(base, x + column, y + row, color);
            column += 1;
        }
        row += 1;
    }
}

fn put_pixel(base: u64, x: u32, y: u32, color: u32) {
    unsafe {
        core::ptr::write_volatile(
            (base + window::PIXELS + u64::from(y * STRIDE + x * 4)) as *mut u32,
            color,
        );
    }
}

fn draw_text(base: u64, x: u32, y: u32, scale: u32, text: &str, color: u32) {
    let mut cursor = x;
    for character in text.chars() {
        draw_char(base, cursor, y, scale, character, color);
        cursor += (font5x7::GLYPH_WIDTH + 1) * scale;
        if cursor >= WIDTH {
            break;
        }
    }
}

fn draw_text_bytes(base: u64, x: u32, y: u32, scale: u32, text: &[u8], color: u32) {
    let mut cursor = x;
    for &byte in text {
        draw_char(base, cursor, y, scale, byte as char, color);
        cursor += (font5x7::GLYPH_WIDTH + 1) * scale;
        if cursor >= WIDTH {
            break;
        }
    }
}

fn draw_char(base: u64, x: u32, y: u32, scale: u32, character: char, color: u32) {
    let glyph = font5x7::glyph(character);
    for (column, bits) in glyph.iter().enumerate() {
        for row in 0..font5x7::GLYPH_HEIGHT {
            if bits & (1 << row) != 0 {
                fill(
                    base,
                    x + column as u32 * scale,
                    y + row * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ivshmem

fn pci_config_address(bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    (u64::from(bus) << 20)
        | (u64::from(device) << 15)
        | (u64::from(function) << 12)
        | (u64::from(offset) & 0xFFC)
}

fn read_u32(address: u64) -> Option<u32> {
    syscall::pci_read32(address)
}

fn write_u32(address: u64, value: u32) -> bool {
    syscall::pci_write32(address, value)
}

fn find_ivshmem() -> Option<(u8, u8, u8)> {
    for bus in 0..=u8::MAX {
        for device in 0..32u8 {
            for function in 0..8u8 {
                let Some(id) = read_u32(pci_config_address(bus, device, function, 0x00)) else {
                    continue;
                };
                if id as u16 == IVSHMEM_VENDOR && (id >> 16) as u16 == IVSHMEM_DEVICE {
                    return Some((bus, device, function));
                }
            }
        }
    }
    None
}

fn open_window() -> Result<u64, u64> {
    let (bus, device, function) = find_ivshmem().ok_or(1u64)?;
    let command_address = pci_config_address(bus, device, function, 0x04);
    let command = read_u32(command_address).ok_or(2u64)?;
    write_u32(command_address, command | 0x2);

    let bar = pci_config_address(bus, device, function, 0x18);
    let high = pci_config_address(bus, device, function, 0x1C);
    let saved_low = read_u32(bar).ok_or(3u64)?;
    let flags = saved_low & 0xF;
    write_u32(bar, 0xFFFF_FFFF);
    write_u32(high, 0xFFFF_FFFF);
    let low_mask = read_u32(bar).ok_or(4u64)? as u64;
    let high_mask = read_u32(high).ok_or(5u64)? as u64;
    let mask = (low_mask & 0xFFFF_FFF0) | (high_mask << 32);
    let size = (!mask).wrapping_add(1);
    let used = window::PIXELS + u64::from(STRIDE) * u64::from(HEIGHT);
    if size < used || size > WINDOW_MAX_SIZE {
        return Err(6u64);
    }
    write_u32(bar, IVSHMEM_BAR as u32 | flags);
    write_u32(high, (IVSHMEM_BAR >> 32) as u32);

    // Map only the header and the pixels we use: the kernel's non-cacheable
    // window is bounded per mapping, and the BAR may be larger than the
    // surface.
    let base = syscall::map_phys_nc(IVSHMEM_BAR, used).ok_or(7u64)?;
    window::write_u32(base, window::FIELD_MAGIC, window::MAGIC);
    window::write_u32(base, window::FIELD_VERSION, window::VERSION);
    window::write_u32(base, window::FIELD_WIDTH, WIDTH);
    window::write_u32(base, window::FIELD_HEIGHT, HEIGHT);
    window::write_u32(base, window::FIELD_STRIDE, STRIDE);
    window::write_u32(base, window::FIELD_FORMAT, window::FORMAT_RGBA8888);
    window::write_u32(base, window::FIELD_FRAME_SEQ, 0);
    window::write_u32(base, window::FIELD_STATE, window::STATE_BLANK);
    window::write_u32(base, window::FIELD_INPUT_HEAD, 0);
    window::write_u32(base, window::FIELD_INPUT_TAIL, 0);
    window::write_u32(base, window::FIELD_SCROLL, 0);
    Ok(base)
}

// ---------------------------------------------------------------------------
// Network client (same window contract as the shell's `wget`)

fn map_net_window(server: u64) -> Option<u64> {
    if !syscall::send(server, &net_ipc::share_request()) {
        return None;
    }
    let reply = wait_reply(server, net_ipc::OP_SHARE_REPLY, 2 * TICKS_PER_SECOND)?;
    let (paddr, size) = net_ipc::share_fields(&reply)?;
    syscall::map_phys(paddr, size)
}

fn write_request(base: u64, text: &str) -> bool {
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

fn wait_reply(server: u64, opcode: u8, timeout_ticks: u64) -> Option<ipc::Message> {
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

fn find_task_id(program: u64) -> Option<u64> {
    let mut tasks = [syscall::TaskInfo {
        id: 0,
        state: 0,
        program: 0,
        argument: 0,
    }; 32];
    let count = syscall::task_list(&mut tasks)?;
    tasks[..count.min(32)]
        .iter()
        .rev()
        .find(|task| {
            task.program == program && task.state != 6 && task.state != 7
        })
        .map(|task| task.id)
}

fn fatal(status: u64, message: &str) -> ! {
    // The browser owns no console, so an early failure exits and the kernel's
    // task-exit line names it. Once the window exists, `paint` shows errors.
    syscall::print("BROWSER: ");
    syscall::print(message);
    syscall::print("\r\n");
    syscall::exit_with_status(status)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit_with_status(0xB0B0)
}
