//! Notes: a separate EL0 process launched from the home screen.
//!
//! The text lives in this process only; persistence to `/sfs` is the next step.

#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../compositor.rs"]
mod compositor;

use compositor::*;
use core::panic::PanicInfo;

const SCREEN_WIDTH: u32 = 640;
const SCREEN_HEIGHT: u32 = 1136;

const COLOR_KEY: u32 = 0xFF2C_2C_32;
const COLOR_KEY_HOT: u32 = 0xFFE08A2E;
const COLOR_KEY_CLEAR: u32 = 0xFFC0392B;
const PAPER: u32 = 0xFFF7_E8_B0;
const INK: u32 = 0xFF20_1A_10;

const ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const KEY: u32 = 58;
const GAP: u32 = 4;
const KEY_Y: u32 = 520;
const STEP: u32 = 78;

fn key_rect(row: usize, index: usize) -> (u32, u32, u32, u32) {
    let text = ROWS[row].as_bytes();
    let width = text.len() as u32 * (KEY + GAP) - GAP;
    let x = (SCREEN_WIDTH - width) / 2 + index as u32 * (KEY + GAP);
    (x, KEY_Y + row as u32 * STEP, KEY, KEY)
}

fn extra_rect(index: usize) -> (u32, u32, u32, u32) {
    let widths = [240_u32, 150, 150];
    let total: u32 = widths.iter().sum::<u32>() + 2 * GAP;
    let mut x = (SCREEN_WIDTH - total) / 2;
    for slot in 0..index {
        x += widths[slot] + GAP;
    }
    (x, KEY_Y + 3 * STEP, widths[index], KEY)
}

fn draw(gpu: u64, note: &[u8]) {
    clear(gpu, 0xFF17_14_0E);
    header(gpu, SCREEN_WIDTH, "Notes");

    rect(gpu, 30, 90, 580, 380, PAPER);
    let mut line = [0_u8; 33];
    let mut line_len = 0;
    let mut row = 0_u32;
    for byte in note.iter().copied() {
        if line_len == 32 || byte == b'\n' || row >= 16 {
            text(
                gpu,
                45,
                112 + row * 22,
                INK,
                core::str::from_utf8(&line[..line_len]).unwrap_or(""),
            );
            row += 1;
            line_len = 0;
            continue;
        }
        line[line_len] = byte;
        line_len += 1;
    }
    if line_len > 0 {
        text(
            gpu,
            45,
            112 + row * 22,
            INK,
            core::str::from_utf8(&line[..line_len]).unwrap_or(""),
        );
    }

    for (row_index, letters) in ROWS.iter().enumerate() {
        for (index, byte) in letters.bytes().enumerate() {
            let (x, y, w, h) = key_rect(row_index, index);
            rect(gpu, x, y, w, h, COLOR_KEY);
            let letter = [byte];
            text_centered(
                gpu,
                x + w / 2,
                y + h / 2 - 4,
                COLOR_WHITE,
                core::str::from_utf8(&letter).unwrap_or(""),
            );
        }
    }
    for index in 0..3 {
        let (x, y, w, h) = extra_rect(index);
        let (label, color) = match index {
            0 => ("SPACE", COLOR_KEY),
            1 => ("DEL", COLOR_KEY_HOT),
            _ => ("CLR", COLOR_KEY_CLEAR),
        };
        rect(gpu, x, y, w, h, color);
        text_centered(gpu, x + w / 2, y + h / 2 - 4, COLOR_WHITE, label);
    }
}

fn tap(note: &mut [u8], len: &mut usize, x: u32, y: u32) {
    for (row_index, letters) in ROWS.iter().enumerate() {
        for (index, byte) in letters.bytes().enumerate() {
            let (bx, by, bw, bh) = key_rect(row_index, index);
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
        let (bx, by, bw, bh) = extra_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            match index {
                0 => {
                    if *len < note.len() {
                        note[*len] = b' ';
                        *len += 1;
                    }
                }
                1 => *len = len.saturating_sub(1),
                _ => *len = 0,
            }
            return;
        }
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let Some(gpu) = connect() else {
        syscall::exit();
    };

    let mut note = [0_u8; 160];
    let mut len = 0usize;
    draw(gpu, &note[..len]);

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
        if message.payload[5] != 0 {
            continue;
        }
        if back_hit(x, y) {
            syscall::exit();
        }
        tap(&mut note, &mut len, x, y);
        draw(gpu, &note[..len]);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
