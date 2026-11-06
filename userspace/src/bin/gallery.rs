//! Gallery: a separate EL0 process launched from the home screen.

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

const TILES: [u32; 9] = [
    0xFFE05A47, 0xFFE0A23A, 0xFF7BBF4A, 0xFF3AA6A0, 0xFF3A6FD8, 0xFF7A4AD8, 0xFFD84A9E,
    0xFF8A8F98, 0xFF3C3C44,
];

fn tile_rect(index: usize) -> (u32, u32, u32, u32) {
    let column = (index % 3) as u32;
    let row = (index / 3) as u32;
    let size = 170;
    let gap = 20;
    let x = (SCREEN_WIDTH - (3 * size + 2 * gap)) / 2 + column * (size + gap);
    let y = 150 + row * (size + gap);
    (x, y, size, size)
}

fn draw(gpu: u64, selected: Option<usize>) {
    if let Some(index) = selected {
        clear(gpu, TILES[index]);
        header(gpu, SCREEN_WIDTH, "Gallery");
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
    header(gpu, SCREEN_WIDTH, "Gallery");
    for index in 0..TILES.len() {
        let (x, y, w, h) = tile_rect(index);
        rect(gpu, x, y, w, h, shade(TILES[index]));
        rect(gpu, x + 3, y + 3, w - 6, h - 6, TILES[index]);
    }
}

fn tap(x: u32, y: u32) -> Option<usize> {
    for index in 0..TILES.len() {
        let (bx, by, bw, bh) = tile_rect(index);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return Some(index);
        }
    }
    None
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let Some(gpu) = connect() else {
        syscall::exit();
    };

    let mut selected: Option<usize> = None;
    draw(gpu, selected);

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
            if selected.is_some() {
                selected = None;
            } else {
                syscall::exit();
            }
            draw(gpu, selected);
            continue;
        }
        match selected {
            Some(_) => selected = None,
            None => selected = tap(x, y),
        }
        draw(gpu, selected);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}
