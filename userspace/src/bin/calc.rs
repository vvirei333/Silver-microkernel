//! Calc: a separate EL0 process launched from the home screen.
//!
//! Draws through the compositor and returns to SilverHome when the header's
//! back button is tapped (the process exits, and the host takes the screen
//! back with a `GUI_HANDOFF`).

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

const COLOR_PANEL: u32 = 0xFF14_14_18;
const COLOR_KEY: u32 = 0xFF2C_2C_32;
const COLOR_KEY_HOT: u32 = 0xFFE08A2E;
const COLOR_KEY_GO: u32 = 0xFF3CB44B;
const COLOR_KEY_CLEAR: u32 = 0xFFC0392B;

const KEYS: [&str; 16] = [
    "7", "8", "9", "/", "4", "5", "6", "*", "1", "2", "3", "-", "0", "C", "=", "+",
];

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
    let len = 20 - index;
    let mut out = [0_u8; 20];
    out[..len].copy_from_slice(&buffer[index..]);
    out
}

fn key_rect(index: usize) -> (u32, u32, u32, u32) {
    let column = (index % 4) as u32;
    let row = (index / 4) as u32;
    let size = 130;
    let gap = 12;
    let x = (SCREEN_WIDTH - (4 * size + 3 * gap)) / 2 + column * (size + gap);
    let y = 470 + row * (size + gap);
    (x, y, size, size)
}

fn draw(gpu: u64, calc: &Calc) {
    clear(gpu, COLOR_PANEL);
    header(gpu, SCREEN_WIDTH, "Calc");

    rect(gpu, 40, 120, 560, 170, 0xFF20_20_26);
    let digits = core::str::from_utf8(&calc.display[..calc.len]).unwrap_or("0");
    let width = digits.len() as u32 * 8 * 5;
    text_scaled(gpu, 580 - width.min(520), 165, COLOR_WHITE, 5, digits);

    for (index, label) in KEYS.iter().enumerate() {
        let (x, y, w, h) = key_rect(index);
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

fn tap(calc: &mut Calc, x: u32, y: u32) {
    for index in 0..KEYS.len() {
        let (bx, by, bw, bh) = key_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            match KEYS[index] {
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

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let Some(gpu) = connect() else {
        syscall::exit();
    };

    let mut calc = Calc::new();
    draw(gpu, &calc);

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
        tap(&mut calc, x, y);
        draw(gpu, &calc);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
