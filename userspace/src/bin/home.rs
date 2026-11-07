//! SilverHome: the single-page home screen and its built-in apps.
//!
//! It owns no pixels. Everything is drawn through the compositor channel
//! (`GUI_CLEAR` / `GUI_RECT` / `GUI_TEXT` / `GUI_TEXT_SCALED`), and the
//! compositor forwards touch samples back as `GUI_POINTER`. Built-in apps
//! (calculator, gallery, notes) live here for now; later they become separate
//! ELFs launched from `/apps`.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const SCREEN_WIDTH: u32 = 640;
const SCREEN_HEIGHT: u32 = 1136;

const ICON_SIZE: u32 = 118;
const ICON_GAP_X: u32 = 26;
const ICON_ROW_STEP: u32 = 174;
const GRID_COLUMNS: usize = 4;
const GRID_X: u32 =
    (SCREEN_WIDTH - (GRID_COLUMNS as u32 * ICON_SIZE + (GRID_COLUMNS as u32 - 1) * ICON_GAP_X)) / 2;
const GRID_Y: u32 = 96;
const DOCK_Y: u32 = 962;
const DOCK_ICON: u32 = 108;
const DOCK_APPS: [usize; 4] = [0, 1, 2, 3];

const APP_LABELS: [&str; 20] = [
    "Calls", "Post", "Browser", "Music", "Chat", "Calendar", "Gallery", "Camera", "Video",
    "Markets", "Maps", "Weather", "Clock", "Calc", "Notes", "Setup", "Films", "Recorder",
    "Compass", "Tasks",
];
const APP_COLORS: [u32; 20] = [
    0xFF3CB44B, 0xFF2E6FD9, 0xFF29A3E8, 0xFFE8467C, 0xFF4CD964, 0xFFE8E8E8, 0xFFF5A623,
    0xFF8E8E93, 0xFFE52D27, 0xFF1F1F1F, 0xFF34A853, 0xFF4A90E2, 0xFF1C1C1E, 0xFF3A3A3C,
    0xFFF7D154, 0xFF8E8E93, 0xFF5AC8FA, 0xFFAF52DE, 0xFF1F1F1F, 0xFFFF3B30,
];

const APP_GALLERY: usize = 6;
const APP_CALC: usize = 13;
const APP_NOTES: usize = 14;

const COLOR_BLACK: u32 = 0xFF00_0000;
const COLOR_WHITE: u32 = 0xFFFF_FFFF;
const COLOR_PANEL: u32 = 0xFF14_14_18;
const COLOR_KEY: u32 = 0xFF2C_2C_32;
const COLOR_KEY_HOT: u32 = 0xFFE08A2E;
const COLOR_KEY_GO: u32 = 0xFF3CB44B;
const COLOR_KEY_CLEAR: u32 = 0xFFC0392B;

const GUI_CLEAR: u8 = 0x10;
const GUI_RECT: u8 = 0x11;
const GUI_TEXT: u8 = 0x12;
const GUI_TEXT_SCALED: u8 = 0x14;
const GUI_POINTER: u8 = 0x13;
const GUI_HANDOFF: u8 = 0x15;
const GUI_SURFACE_BEGIN: u8 = 0x16;
const GUI_SURFACE_PRESENT: u8 = 0x17;
const GUI_BUTTON_PRESSED: u8 = 2;

#[derive(Copy, Clone)]
struct Screen {
    mode: u8, // 0 home, 1 calc, 2 gallery, 3 notes, 4 generic app
    app: usize,
}

impl Screen {
    const fn home() -> Self {
        Self { mode: 0, app: 0 }
    }
}

struct Calc {
    acc: i64,
    value: i64,
    op: u8,
    display: [u8; 20],
    len: usize,
}

impl Calc {
    const fn new() -> Self {
        Self {
            acc: 0,
            value: 0,
            op: 0,
            display: [b'0'; 20],
            len: 1,
        }
    }

    fn show(&mut self, value: i64) {
        let text = format_i64(value);
        let len = text.iter().position(|&byte| byte == 0).unwrap_or(text.len());
        self.display[..len].copy_from_slice(&text[..len]);
        self.len = len;
    }

    fn reset(&mut self) {
        self.acc = 0;
        self.value = 0;
        self.op = 0;
        self.show(0);
    }

    fn digit(&mut self, digit: i64) {
        self.value = self.value.saturating_mul(10).saturating_add(digit);
        self.show(self.value);
    }

    fn operator(&mut self, op: u8) {
        if self.op != 0 {
            self.acc = apply(self.acc, self.value, self.op);
        } else {
            self.acc = self.value;
        }
        self.value = 0;
        self.op = op;
        self.show(self.acc);
    }

    fn equals(&mut self) {
        self.acc = apply(self.acc, self.value, self.op);
        self.value = 0;
        self.op = 0;
        self.show(self.acc);
    }
}

fn apply(acc: i64, value: i64, op: u8) -> i64 {
    match op {
        b'+' => acc.saturating_add(value),
        b'-' => acc.saturating_sub(value),
        b'*' => acc.saturating_mul(value),
        b'/' => {
            if value == 0 {
                0
            } else {
                acc / value
            }
        }
        _ => value,
    }
}

fn format_i64(value: i64) -> [u8; 20] {
    let mut buffer = [0_u8; 20];
    let negative = value < 0;
    let mut magnitude = if negative {
        value.unsigned_abs()
    } else {
        value as u64
    };
    let mut index = 20;
    loop {
        index -= 1;
        buffer[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        index -= 1;
        buffer[index] = b'-';
    }
    // Left-align the result so callers can use `len` starting at offset 0.
    let len = 20 - index;
    let mut out = [0_u8; 20];
    out[..len].copy_from_slice(&buffer[index..]);
    out
}

// ---------------------------------------------------------------------------
// Compositor channel
// ---------------------------------------------------------------------------

fn send(server: u64, payload: &[u8]) {
    let mut data = [0_u8; ipc::PAYLOAD_SIZE];
    let len = payload.len().min(ipc::PAYLOAD_SIZE);
    data[..len].copy_from_slice(&payload[..len]);
    let _ = syscall::send(server, &ipc::Message::new(0, &data[..len]));
}

fn clear(gpu: u64, color: u32) {
    let mut payload = [0_u8; 5];
    payload[0] = GUI_CLEAR;
    payload[1..5].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

fn surface_begin(gpu: u64, color: u32) {
    let mut payload = [0_u8; 5];
    payload[0] = GUI_SURFACE_BEGIN;
    payload[1..5].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

fn surface_present(gpu: u64, seq: u64) {
    let mut payload = [0_u8; 9];
    payload[0] = GUI_SURFACE_PRESENT;
    payload[1..9].copy_from_slice(&seq.to_le_bytes());
    send(gpu, &payload);
}

static mut FRAME_SEQ: u64 = 0;

fn next_frame_seq() -> u64 {
    unsafe {
        FRAME_SEQ = FRAME_SEQ.wrapping_add(1);
        FRAME_SEQ
    }
}

fn rect(gpu: u64, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let mut payload = [0_u8; 13];
    payload[0] = GUI_RECT;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..7].copy_from_slice(&(w as u16).to_le_bytes());
    payload[7..9].copy_from_slice(&(h as u16).to_le_bytes());
    payload[9..13].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

fn text(gpu: u64, x: u32, y: u32, color: u32, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(ipc::PAYLOAD_SIZE - 10);
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = GUI_TEXT;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..9].copy_from_slice(&color.to_le_bytes());
    payload[9] = len as u8;
    payload[10..10 + len].copy_from_slice(&bytes[..len]);
    send(gpu, &payload);
}

fn text_scaled(gpu: u64, x: u32, y: u32, color: u32, scale: u32, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(ipc::PAYLOAD_SIZE - 11);
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = GUI_TEXT_SCALED;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..9].copy_from_slice(&color.to_le_bytes());
    payload[9] = scale as u8;
    payload[10] = len as u8;
    payload[11..11 + len].copy_from_slice(&bytes[..len]);
    send(gpu, &payload);
}

fn text_centered(gpu: u64, center_x: u32, y: u32, color: u32, value: &str) {
    let width = value.len() as u32 * 8;
    text(gpu, center_x.saturating_sub(width / 2), y, color, value);
}

fn text_centered_scaled(gpu: u64, center_x: u32, y: u32, color: u32, scale: u32, value: &str) {
    let width = value.len() as u32 * 8 * scale;
    text_scaled(gpu, center_x.saturating_sub(width / 2), y, color, scale, value);
}

fn shade(color: u32) -> u32 {
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    let scale = |value: u32| (value * 3) / 4;
    0xFF00_0000 | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

// ---------------------------------------------------------------------------
// Home page
// ---------------------------------------------------------------------------

fn grid_position(index: usize) -> (u32, u32) {
    let column = (index % GRID_COLUMNS) as u32;
    let row = (index / GRID_COLUMNS) as u32;
    (
        GRID_X + column * (ICON_SIZE + ICON_GAP_X),
        GRID_Y + row * ICON_ROW_STEP,
    )
}

fn dock_position(slot: usize) -> (u32, u32) {
    let total = DOCK_APPS.len() as u32 * DOCK_ICON + (DOCK_APPS.len() as u32 - 1) * 30;
    let x = (SCREEN_WIDTH - total) / 2 + slot as u32 * (DOCK_ICON + 30);
    (x, DOCK_Y + 20)
}

fn draw_status_bar(gpu: u64) {
    rect(gpu, 0, 0, SCREEN_WIDTH, 48, COLOR_BLACK);
    text(gpu, 12, 16, COLOR_WHITE, "9:41");
    text_centered(gpu, SCREEN_WIDTH / 2, 16, COLOR_WHITE, "Silver");
    rect(gpu, SCREEN_WIDTH - 60, 16, 44, 16, 0xFF30D158);
}

fn draw_home(gpu: u64, pressed: Option<usize>) {
    surface_begin(gpu, COLOR_BLACK);
    let bands = 16_u32;
    for band in 0..bands {
        let t = band * 255 / bands;
        let color = 0xFF00_0000 | ((12 + t / 6) << 16) | ((48 + t / 3) << 8) | (96 + t / 2);
        rect(
            gpu,
            0,
            band * SCREEN_HEIGHT / bands,
            SCREEN_WIDTH,
            SCREEN_HEIGHT / bands + 1,
            color,
        );
    }

    draw_status_bar(gpu);

    for index in 0..APP_LABELS.len() {
        let (x, y) = grid_position(index);
        let face = if pressed == Some(index) {
            shade(APP_COLORS[index])
        } else {
            APP_COLORS[index]
        };
        rect(gpu, x, y, ICON_SIZE, ICON_SIZE, shade(APP_COLORS[index]));
        rect(gpu, x + 2, y + 2, ICON_SIZE - 4, ICON_SIZE - 4, face);
        text_centered(
            gpu,
            x + ICON_SIZE / 2,
            y + ICON_SIZE + 12,
            COLOR_WHITE,
            APP_LABELS[index],
        );
    }

    rect(gpu, 0, DOCK_Y, SCREEN_WIDTH, SCREEN_HEIGHT - DOCK_Y, 0xFF10_1A2A);
    for (slot, &app) in DOCK_APPS.iter().enumerate() {
        let (x, y) = dock_position(slot);
        let face = if pressed == Some(app) {
            shade(APP_COLORS[app])
        } else {
            APP_COLORS[app]
        };
        rect(gpu, x, y, DOCK_ICON, DOCK_ICON, shade(APP_COLORS[app]));
        rect(gpu, x + 2, y + 2, DOCK_ICON - 4, DOCK_ICON - 4, face);
    }
    surface_present(gpu, next_frame_seq());
}

fn app_at(x: u32, y: u32) -> Option<usize> {
    for index in 0..APP_LABELS.len() {
        let (bx, by) = grid_position(index);
        if x >= bx && x < bx + ICON_SIZE && y >= by && y < by + ICON_SIZE + 20 {
            return Some(index);
        }
    }
    for (slot, &app) in DOCK_APPS.iter().enumerate() {
        let (bx, by) = dock_position(slot);
        if x >= bx && x < bx + DOCK_ICON && y >= by && y < by + DOCK_ICON {
            return Some(app);
        }
    }
    None
}

fn notify_howl(app: usize) {
    let Some(howl) = syscall::lookup("howl") else {
        return;
    };
    let label = APP_LABELS[app].as_bytes();
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = GUI_BUTTON_PRESSED;
    let len = label.len().min(ipc::PAYLOAD_SIZE - 1);
    payload[1..1 + len].copy_from_slice(&label[..len]);
    let _ = syscall::try_send(howl, &ipc::Message::new(0, &payload[..1 + len]));
}

// ---------------------------------------------------------------------------
// Calculator
// ---------------------------------------------------------------------------

const CALC_KEYS: [&str; 16] = [
    "7", "8", "9", "/", "4", "5", "6", "*", "1", "2", "3", "-", "0", "C", "=", "+",
];

fn calc_key_rect(index: usize) -> (u32, u32, u32, u32) {
    let column = (index % 4) as u32;
    let row = (index / 4) as u32;
    let size = 130;
    let gap = 12;
    let x = (SCREEN_WIDTH - (4 * size + 3 * gap)) / 2 + column * (size + gap);
    let y = 470 + row * (size + gap);
    (x, y, size, size)
}

fn draw_calc(gpu: u64, calc: &Calc) {
    clear(gpu, COLOR_PANEL);
    draw_status_bar(gpu);
    text_centered_scaled(gpu, SCREEN_WIDTH / 2, 80, COLOR_WHITE, 2, "Calculator");

    rect(gpu, 40, 130, 560, 180, 0xFF20_20_26);
    let digits = core::str::from_utf8(&calc.display[..calc.len]).unwrap_or("0");
    let width = digits.len() as u32 * 8 * 5;
    text_scaled(gpu, 580 - width.min(520), 190, COLOR_WHITE, 5, digits);

    for (index, label) in CALC_KEYS.iter().enumerate() {
        let (x, y, w, h) = calc_key_rect(index);
        let color = match *label {
            "=" => COLOR_KEY_GO,
            "C" => COLOR_KEY_CLEAR,
            "+" | "-" | "*" | "/" => COLOR_KEY_HOT,
            _ => COLOR_KEY,
        };
        rect(gpu, x, y, w, h, color);
        text_centered_scaled(gpu, x + w / 2, y + h / 2 - 16, COLOR_WHITE, 4, label);
    }
}

fn calc_tap(calc: &mut Calc, x: u32, y: u32) {
    for index in 0..CALC_KEYS.len() {
        let (bx, by, bw, bh) = calc_key_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            match CALC_KEYS[index] {
                "C" => calc.reset(),
                "=" => calc.equals(),
                "+" => calc.operator(b'+'),
                "-" => calc.operator(b'-'),
                "*" => calc.operator(b'*'),
                "/" => calc.operator(b'/'),
                digit => calc.digit((digit.as_bytes()[0] - b'0') as i64),
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Gallery
// ---------------------------------------------------------------------------

const GALLERY_COLORS: [u32; 9] = [
    0xFFE05A47, 0xFFE0A23A, 0xFF7BBF4A, 0xFF3AA6A0, 0xFF3A6FD8, 0xFF7A4AD8, 0xFFD84A9E,
    0xFF8A8F98, 0xFF3C3C44,
];

fn gallery_rect(index: usize) -> (u32, u32, u32, u32) {
    let column = (index % 3) as u32;
    let row = (index / 3) as u32;
    let size = 170;
    let gap = 20;
    let x = (SCREEN_WIDTH - (3 * size + 2 * gap)) / 2 + column * (size + gap);
    let y = 200 + row * (size + gap);
    (x, y, size, size)
}

fn draw_gallery(gpu: u64, selected: Option<usize>) {
    if let Some(index) = selected {
        clear(gpu, GALLERY_COLORS[index]);
        draw_status_bar(gpu);
        text_centered_scaled(
            gpu,
            SCREEN_WIDTH / 2,
            SCREEN_HEIGHT / 2,
            COLOR_WHITE,
            3,
            "Tap to return",
        );
        return;
    }
    clear(gpu, 0xFF0A_0A_0E);
    draw_status_bar(gpu);
    text_centered_scaled(gpu, SCREEN_WIDTH / 2, 110, COLOR_WHITE, 2, "Gallery");
    for index in 0..GALLERY_COLORS.len() {
        let (x, y, w, h) = gallery_rect(index);
        rect(gpu, x, y, w, h, shade(GALLERY_COLORS[index]));
        rect(gpu, x + 3, y + 3, w - 6, h - 6, GALLERY_COLORS[index]);
    }
}

fn gallery_tap(x: u32, y: u32) -> Option<usize> {
    for index in 0..GALLERY_COLORS.len() {
        let (bx, by, bw, bh) = gallery_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return Some(index);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Notes (on-screen keyboard)
// ---------------------------------------------------------------------------

const NOTE_ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const NOTE_KEY: u32 = 58;
const NOTE_GAP: u32 = 4;
const NOTE_Y: u32 = 520;
const NOTE_STEP: u32 = 78;

fn note_key_rect(row: usize, index: usize) -> (u32, u32, u32, u32) {
    let text = NOTE_ROWS[row].as_bytes();
    let width = text.len() as u32 * (NOTE_KEY + NOTE_GAP) - NOTE_GAP;
    let x = (SCREEN_WIDTH - width) / 2 + index as u32 * (NOTE_KEY + NOTE_GAP);
    (x, NOTE_Y + row as u32 * NOTE_STEP, NOTE_KEY, NOTE_KEY)
}

fn note_extra_rect(index: usize) -> (u32, u32, u32, u32) {
    let widths = [240_u32, 150, 150];
    let total: u32 = widths.iter().sum::<u32>() + 2 * NOTE_GAP;
    let mut x = (SCREEN_WIDTH - total) / 2;
    for slot in 0..index {
        x += widths[slot] + NOTE_GAP;
    }
    (x, NOTE_Y + 3 * NOTE_STEP, widths[index], NOTE_KEY)
}

fn draw_notes(gpu: u64, note: &[u8]) {
    clear(gpu, 0xFF17_14_0E);
    draw_status_bar(gpu);
    text_centered_scaled(gpu, SCREEN_WIDTH / 2, 80, COLOR_WHITE, 2, "Notes");
    rect(gpu, 30, 130, 580, 330, 0xFFF7_E8_B0);

    let visible = core::str::from_utf8(note).unwrap_or("");
    let mut line = [0_u8; 33];
    let mut line_len = 0;
    let mut row = 0_u32;
    for byte in visible.bytes() {
        if line_len == 32 || byte == b'\n' {
            text(gpu, 45, 150 + row * 22, 0xFF20_1A_10, core::str::from_utf8(&line[..line_len]).unwrap_or(""));
            row += 1;
            line_len = 0;
            continue;
        }
        line[line_len] = byte;
        line_len += 1;
    }
    if line_len > 0 {
        text(gpu, 45, 150 + row * 22, 0xFF20_1A_10, core::str::from_utf8(&line[..line_len]).unwrap_or(""));
    }

    for (row_index, text) in NOTE_ROWS.iter().enumerate() {
        for (index, byte) in text.bytes().enumerate() {
            let (x, y, w, h) = note_key_rect(row_index, index);
            rect(gpu, x, y, w, h, COLOR_KEY);
            let letter = [byte];
            text_centered(gpu, x + w / 2, y + h / 2 - 4, COLOR_WHITE, core::str::from_utf8(&letter).unwrap_or(""));
        }
    }
    for index in 0..3 {
        let (x, y, w, h) = note_extra_rect(index);
        let (label, color) = match index {
            0 => ("SPACE", COLOR_KEY),
            1 => ("DEL", COLOR_KEY_HOT),
            _ => ("CLR", COLOR_KEY_CLEAR),
        };
        rect(gpu, x, y, w, h, color);
        text_centered(gpu, x + w / 2, y + h / 2 - 4, COLOR_WHITE, label);
    }
}

fn notes_tap(note: &mut [u8], len: &mut usize, x: u32, y: u32) {
    for (row_index, text) in NOTE_ROWS.iter().enumerate() {
        for (index, byte) in text.bytes().enumerate() {
            let (bx, by, bw, bh) = note_key_rect(row_index, index);
            if x >= bx && x < bx + bw && y >= by && y < by + bh {
                if *len < note.len() {
                    note[*len] = byte;
                    *len += 1;
                }
                return;
            }
        }
    }
    for index in 0..3 {
        let (bx, by, bw, bh) = note_extra_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            match index {
                0 => {
                    if *len < note.len() {
                        note[*len] = b' ';
                        *len += 1;
                    }
                }
                1 => {
                    *len = len.saturating_sub(1);
                }
                _ => *len = 0,
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Generic app placeholder
// ---------------------------------------------------------------------------

fn draw_app(gpu: u64, app: usize) {
    clear(gpu, shade(APP_COLORS[app]));
    draw_status_bar(gpu);
    rect(gpu, 0, 48, SCREEN_WIDTH, SCREEN_HEIGHT - 48, APP_COLORS[app]);
    text_centered_scaled(gpu, SCREEN_WIDTH / 2, 110, COLOR_WHITE, 2, APP_LABELS[app]);
    rect(gpu, (SCREEN_WIDTH - 200) / 2, 360, 200, 200, shade(APP_COLORS[app]));
    rect(gpu, (SCREEN_WIDTH - 196) / 2, 362, 196, 196, APP_COLORS[app]);
    text_centered_scaled(gpu, SCREEN_WIDTH / 2, 760, COLOR_WHITE, 2, "Tap to return");
}

// ---------------------------------------------------------------------------
// App launching
// ---------------------------------------------------------------------------

/// Hands the child the screen before waiting for it and takes it back
/// afterwards. Returns false when the image cannot be spawned, so the caller
/// can fall back to the in-process screen.
fn launch_app(gpu: u64, path: &str) -> bool {
    let Some(cap) = syscall::spawn_path(path) else {
        return false;
    };
    let Some(task) = syscall::cap_get_task_id(cap) else {
        let _ = syscall::wait(cap);
        return false;
    };
    handoff_ui(gpu, task);
    let _ = syscall::wait(cap);
    handoff_ui(gpu, 0);
    true
}

/// Tells the compositor who may draw and receive pointer samples next.
fn handoff_ui(gpu: u64, task: u64) {
    let mut payload = [0_u8; 9];
    payload[0] = GUI_HANDOFF;
    payload[1..9].copy_from_slice(&task.to_le_bytes());
    send(gpu, &payload);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut gpu = None;
    for _ in 0..50 {
        if let Some(id) = syscall::lookup("gpu_driver") {
            gpu = Some(id);
            break;
        }
        syscall::sleep(100);
    }
    let Some(gpu) = gpu else {
        syscall::exit();
    };

    draw_home(gpu, None);

    for _ in 0..20 {
        if syscall::lookup("howl").is_some() {
            break;
        }
        syscall::sleep(100);
    }
    notify_howl(15);

    let mut screen = Screen::home();
    let mut pressed: Option<usize> = None;
    let mut calc = Calc::new();
    let mut note = [0_u8; 120];
    let mut note_len = 0usize;
    let mut gallery_selected: Option<usize> = None;

    loop {
        let mut message = ipc::Message::new(0, &[]);
        if !syscall::receive(&mut message) {
            continue;
        }
        if message.payload[0] != GUI_POINTER {
            continue;
        }
        let x = u32::from(u16::from_le_bytes([message.payload[1], message.payload[2]]));
        let y = u32::from(u16::from_le_bytes([message.payload[3], message.payload[4]]));
        let down = message.payload[5] != 0;

        match screen.mode {
            0 => {
                if down {
                    let hit = app_at(x, y);
                    if hit != pressed {
                        pressed = hit;
                        draw_home(gpu, pressed);
                    }
                } else if let Some(app) = pressed.take() {
                    notify_howl(app);
                    screen.app = app;
                    // The three built-ins are real processes now. If the image
                    // cannot be launched, fall back to the in-process screen.
                    let (disk, initramfs) = match app {
                        APP_CALC => ("/sfs/apps/calc.elf", "/apps/calc.elf"),
                        APP_GALLERY => ("/sfs/apps/gallery.elf", "/apps/gallery.elf"),
                        APP_NOTES => ("/sfs/apps/notes.elf", "/apps/notes.elf"),
                        _ => ("", ""),
                    };
                    let launched = !disk.is_empty()
                        && (launch_app(gpu, disk) || launch_app(gpu, initramfs));
                    if launched {
                        screen.mode = 0;
                        pressed = None;
                        draw_home(gpu, None);
                        continue;
                    }
                    match app {
                        APP_CALC => screen.mode = 1,
                        APP_GALLERY => {
                            screen.mode = 2;
                            gallery_selected = None;
                        }
                        APP_NOTES => screen.mode = 3,
                        _ => screen.mode = 4,
                    }
                    match screen.mode {
                        1 => draw_calc(gpu, &calc),
                        2 => draw_gallery(gpu, None),
                        3 => draw_notes(gpu, &note[..note_len]),
                        _ => draw_app(gpu, app),
                    }
                }
            }
            1 => {
                if !down {
                    calc_tap(&mut calc, x, y);
                    draw_calc(gpu, &calc);
                }
            }
            2 => {
                if !down {
                    match gallery_selected {
                        Some(_) => gallery_selected = None,
                        None => gallery_selected = gallery_tap(x, y),
                    }
                    draw_gallery(gpu, gallery_selected);
                }
            }
            3 => {
                if !down {
                    notes_tap(&mut note, &mut note_len, x, y);
                    draw_notes(gpu, &note[..note_len]);
                }
            }
            _ => {
                if !down {
                    screen.mode = 0;
                    draw_home(gpu, None);
                }
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
