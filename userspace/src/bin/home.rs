//! SilverHome: the single-page home screen client of the Silver compositor.
//!
//! It owns no pixels. Everything is drawn through the compositor channel
//! (`GUI_CLEAR` / `GUI_RECT` / `GUI_TEXT`), and the compositor forwards touch
//! samples back as `GUI_POINTER`. This is the layer that will later grow pages,
//! folders, gestures and app launching; the compositor stays a dumb blitter.

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

const COLOR_BLACK: u32 = 0xFF00_0000;
const COLOR_WHITE: u32 = 0xFFFF_FFFF;

const GUI_CLEAR: u8 = 0x10;
const GUI_RECT: u8 = 0x11;
const GUI_TEXT: u8 = 0x12;
const GUI_POINTER: u8 = 0x13;
const GUI_BUTTON_PRESSED: u8 = 2;

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

fn text_centered(gpu: u64, center_x: u32, y: u32, color: u32, value: &str) {
    let width = value.len() as u32 * 8;
    text(gpu, center_x.saturating_sub(width / 2), y, color, value);
}

fn shade(color: u32) -> u32 {
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    let scale = |value: u32| (value * 3) / 4;
    0xFF00_0000 | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

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

fn draw_home(gpu: u64, pressed: Option<usize>) {
    // Wallpaper: a few solid bands approximate a soft vertical gradient.
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

    // Status bar.
    rect(gpu, 0, 0, SCREEN_WIDTH, 48, COLOR_BLACK);
    text(gpu, 12, 16, COLOR_WHITE, "9:41");
    text_centered(gpu, SCREEN_WIDTH / 2, 16, COLOR_WHITE, "Silver");
    rect(gpu, SCREEN_WIDTH - 60, 16, 44, 16, 0xFF30D158);

    // App grid: darker border, lighter face, label underneath.
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

    // Dock.
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
}

/// A single-app screen: full-bleed app colour, title, big icon and a hint.
/// Tapping anywhere returns to the home page.
fn draw_app(gpu: u64, app: usize) {
    clear(gpu, shade(APP_COLORS[app]));
    rect(gpu, 0, 0, SCREEN_WIDTH, 48, COLOR_BLACK);
    text(gpu, 12, 16, COLOR_WHITE, "9:41");
    rect(gpu, 0, 48, SCREEN_WIDTH, SCREEN_HEIGHT - 48, APP_COLORS[app]);
    text_centered(gpu, SCREEN_WIDTH / 2, 110, COLOR_WHITE, APP_LABELS[app]);
    rect(gpu, (SCREEN_WIDTH - 200) / 2, 360, 200, 200, shade(APP_COLORS[app]));
    rect(gpu, (SCREEN_WIDTH - 196) / 2, 362, 196, 196, APP_COLORS[app]);
    text_centered(gpu, SCREEN_WIDTH / 2, 760, COLOR_WHITE, "Tap to return");
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

    // Boot beacon: Howl owns the console and prints app notifications, so a
    // synthetic one proves the client drew and the IPC path is live.
    for _ in 0..20 {
        if syscall::lookup("howl").is_some() {
            break;
        }
        syscall::sleep(100);
    }
    notify_howl(15); // "Settings" is only a marker at this point

    let mut pressed: Option<usize> = None;
    let mut open_app: Option<usize> = None;

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

        if open_app.is_some() {
            if !down {
                open_app = None;
                pressed = None;
                draw_home(gpu, None);
            }
            continue;
        }

        if down {
            let hit = app_at(x, y);
            if hit != pressed {
                pressed = hit;
                draw_home(gpu, pressed);
            }
        } else if let Some(app) = pressed.take() {
            open_app = Some(app);
            draw_app(gpu, app);
            notify_howl(app);
        } else {
            draw_home(gpu, None);
        }
    }}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
