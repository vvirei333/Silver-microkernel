//! Ring 3 graphics driver: a static "engineer menu" on QEMU `virt`.
//!
//! `virt` has no PL111 (that LCD controller lives on realview/vexpress/
//! versatile boards), so this driver uses QEMU's native `ramfb` device instead:
//! a plain framebuffer that lives in guest RAM and is handed to the host
//! display through the firmware-configuration (`fw_cfg`) interface. The guest
//! allocates a contiguous frame buffer with `alloc_dma`, writes a 28-byte
//! `RAMFBCfg` descriptor to the `etc/ramfb` fw_cfg file, and then simply
//! paints pixels into that RAM; QEMU's display surface reads them back.
//!
//! Only `map_phys` (for the fw_cfg MMIO registers) and `alloc_dma` (for the
//! frame buffer and the DMA scratch page) are used. `map_phys` refuses DRAM,
//! which is exactly why the frame buffer itself comes from `alloc_dma` — the
//! same would hold for a real PL111: registers are MMIO, the scan-out buffer
//! is RAM.

#![no_std]
#![no_main]

#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../input_proto.rs"]
mod input_proto;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

// ---------------------------------------------------------------------------
// fw_cfg MMIO (QEMU `virt`)
// ---------------------------------------------------------------------------

/// fw_cfg base address on the `virt` machine. Layout (see QEMU
/// `fw_cfg_init_mem_dma`):
///   +0x00  data register (8 bytes wide)
///   +0x08  selector register (16-bit, big-endian)
///   +0x10  DMA address register (64-bit, big-endian)
const FW_CFG_BASE: u64 = 0x0902_0000;

/// fw_cfg DMA control bits (FWCfgDmaAccess.control).
const FW_CFG_DMA_CTL_ERROR: u32 = 0x01;
const FW_CFG_DMA_CTL_READ: u32 = 0x02;
const FW_CFG_DMA_CTL_SKIP: u32 = 0x04;
const FW_CFG_DMA_CTL_SELECT: u32 = 0x08;
const FW_CFG_DMA_CTL_WRITE: u32 = 0x10;

/// Well-known selector of the file directory item.
const FW_CFG_FILE_DIR_KEY: u16 = 0x19;

/// One directory entry: `size` (u32 BE), `select` (u16 BE), `reserved` (u16),
/// then a 56-byte NUL-padded name.
const FW_CFG_FILE_ENTRY_SIZE: u32 = 64;

// ---------------------------------------------------------------------------
// Display geometry
// ---------------------------------------------------------------------------

const SCREEN_WIDTH: u32 = 640;
const SCREEN_HEIGHT: u32 = 480;
const BYTES_PER_PIXEL: u32 = 4;
const SCREEN_BYTES: u64 = SCREEN_WIDTH as u64 * SCREEN_HEIGHT as u64 * BYTES_PER_PIXEL as u64;

/// DRM fourcc `ARGB8888` = fourcc_code('A','R','2','4'), big-endian on the wire.
const FOURCC_ARGB8888: u32 = 0x3432_5241;

// Palette (0xAARRGGBB little-endian word: B, G, R, A bytes).
const COLOR_BLACK: u32 = 0xFF00_0000;
const COLOR_WHITE: u32 = 0xFFFF_FFFF;
const COLOR_GRAY: u32 = 0xFF80_8080;
const COLOR_LIGHT_GRAY: u32 = 0xFFC0_C0C0;
const COLOR_DARK_GRAY: u32 = 0xFF40_4040;
const COLOR_BLUE: u32 = 0xFF00_5A_B5;
/// Pressed-button fill (dark blue), shown while the pointer holds a button.
const COLOR_PRESSED_BLUE: u32 = 0xFF00_33_66;

/// Layout of the 8192-byte DMA scratch allocation.
const SCRATCH_SIZE: u64 = 8192;
const SCRATCH_DMA_ACCESS_OFF: u64 = 0; // 16-byte FWCfgDmaAccess
const SCRATCH_DIR_OFF: u64 = 64; // file-directory read buffer
const SCRATCH_DIR_CAPACITY: u64 = 4096;
const SCRATCH_CONFIG_OFF: u64 = 4200; // 28-byte RAMFBCfg write buffer

/// The mapped frame buffer for the drawing helpers below.
#[derive(Copy, Clone)]
struct Framebuffer {
    vaddr: u64,
    width: u32,
    height: u32,
}

static mut FRAMEBUFFER: Option<Framebuffer> = None;

/// Live pointer state, updated from [`input_proto::InputMsg`] samples.
#[derive(Copy, Clone)]
struct UiState {
    cursor_x: u16,
    cursor_y: u16,
    /// Button index held by the left button, or `None` while released.
    pressed_btn: Option<usize>,
}

static mut UI: UiState = UiState {
    cursor_x: SCREEN_WIDTH as u16 / 2,
    cursor_y: SCREEN_HEIGHT as u16 / 2,
    pressed_btn: None,
};

/// Center 3 x 2 button grid geometry (shared by drawing and hit-testing).
const BUTTON_W: u32 = 120;
const BUTTON_H: u32 = 60;
const BUTTON_GAP: u32 = 16;
const BUTTON_GRID_X: u32 = (SCREEN_WIDTH - (3 * BUTTON_W + 2 * BUTTON_GAP)) / 2;
const BUTTON_GRID_Y: u32 = 150;
const BUTTON_LABELS: [&str; 6] = ["Phone", "SMS", "Web", "Media", "Power", "Settings"];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::sleep(100);
    syscall::print("GPU DRIVER STARTED\r\n");
    // The input driver finds us by this name to deliver pointer samples.
    if !syscall::register("gpu_driver") {
        fail("GPU DRIVER: REGISTER FAILED\r\n");
    }

    let Some((fb_vaddr, fb_paddr)) = syscall::alloc_dma(SCREEN_BYTES) else {
        fail("GPU DRIVER: FRAMEBUFFER ALLOCATION FAILED\r\n");
    };
    let Some(fw_vaddr) = syscall::map_phys(FW_CFG_BASE, 0x1000) else {
        fail("GPU DRIVER: FW_CFG MAP FAILED\r\n");
    };
    let Some((scratch_vaddr, scratch_paddr)) = syscall::alloc_dma(SCRATCH_SIZE) else {
        fail("GPU DRIVER: SCRATCH ALLOCATION FAILED\r\n");
    };

    unsafe {
        FRAMEBUFFER = Some(Framebuffer {
            vaddr: fb_vaddr,
            width: SCREEN_WIDTH,
            height: SCREEN_HEIGHT,
        });
    }

    let Some(ramfb_selector) = find_ramfb_selector(fw_vaddr, scratch_vaddr, scratch_paddr) else {
        fail("GPU DRIVER: etc/ramfb NOT FOUND\r\n");
    };
    syscall::print("GPU DRIVER: etc/ramfb selector 0x");
    syscall::print_hex_u64(u64::from(ramfb_selector));
    syscall::print("\r\n");

    configure_ramfb(
        fw_vaddr,
        scratch_vaddr,
        scratch_paddr,
        ramfb_selector,
        fb_paddr,
    );

    syscall::print("GPU DRIVER: FRAMEBUFFER 0x");
    syscall::print_hex_u64(fb_paddr);
    syscall::print(" (");
    syscall::print_dec_u64(u64::from(SCREEN_WIDTH));
    syscall::print("x");
    syscall::print_dec_u64(u64::from(SCREEN_HEIGHT));
    syscall::print(")\r\n");

    let (cursor_x, cursor_y, pressed_btn) = unsafe {
        let ui = core::ptr::addr_of!(UI);
        ((*ui).cursor_x, (*ui).cursor_y, (*ui).pressed_btn)
    };
    render(cursor_x, cursor_y, pressed_btn);

    syscall::print("GPU DRIVER: ENGINEER MENU DRAWN\r\n");

    // The frame buffer must stay mapped for the life of the display; exiting
    // would release the DMA pages and leave ramfb scanning freed RAM. Block on
    // pointer samples from the input driver and repaint on each one.
    serve();
}

/// Blocks on [`input_proto::InputMsg`] samples and repaints the menu and cursor
/// after each report boundary.
fn serve() -> ! {
    loop {
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive(&mut message) {
            if let Some(input) = input_proto::decode(&message) {
                handle_pointer(input);
            }
        }
    }
}

/// Applies one pointer sample: moves the cursor, tracks button press/release,
/// and repaints the whole frame so the cursor and button state stay consistent.
fn handle_pointer(input: input_proto::InputMsg) {
    let ui = unsafe { &mut *core::ptr::addr_of_mut!(UI) };
    ui.cursor_x = input.x;
    ui.cursor_y = input.y;

    if input.left_clicked {
        // Pressing over a button selects it; releasing leaves it selected.
        if let Some(index) = button_at(input.x, input.y) {
            ui.pressed_btn = Some(index);
        }
    } else if ui.pressed_btn.take().is_some() {
        // The button reverts to its normal colour because `render` no longer
        // sees it as pressed; report the click.
        syscall::print("Button clicked!\r\n");
    }

    render(ui.cursor_x, ui.cursor_y, ui.pressed_btn);
}

// ---------------------------------------------------------------------------
// fw_cfg DMA
// ---------------------------------------------------------------------------

/// Runs one fw_cfg DMA transfer. `control`/`length`/`address` are the native
/// values; they are encoded big-endian into the FWCfgDmaAccess struct and the
/// transfer is triggered through the big-endian DMA address register.
fn fw_cfg_dma(
    fw_vaddr: u64,
    scratch_vaddr: u64,
    scratch_paddr: u64,
    control: u32,
    length: u32,
    address: u64,
) -> bool {
    // FWCfgDmaAccess: { control: u32 BE, length: u32 BE, address: u64 BE }.
    write_be32(scratch_vaddr + SCRATCH_DMA_ACCESS_OFF, control);
    write_be32(scratch_vaddr + SCRATCH_DMA_ACCESS_OFF + 4, length);
    write_be64(scratch_vaddr + SCRATCH_DMA_ACCESS_OFF + 8, address);

    let access_paddr = scratch_paddr + SCRATCH_DMA_ACCESS_OFF;
    // The struct must be in RAM before QEMU reads it.
    unsafe { core::arch::asm!("dmb sy", options(nostack)) };

    // DMA address register (64-bit big-endian at fw_cfg + 0x10). QEMU only
    // accepts a single 4- or 8-byte store here — byte writes are rejected and
    // surface as an external abort — and the register is big-endian, so store
    // the byte-swapped value in one 8-byte access. The store triggers the
    // transfer.
    unsafe {
        core::ptr::write_volatile((fw_vaddr + 0x10) as *mut u64, access_paddr.swap_bytes());
    }

    // QEMU runs the transfer synchronously and writes the result control word
    // back into the struct. Bit 0 (ERROR) set means the item was read-only or
    // the range was rejected.
    let result = read_be32(scratch_vaddr + SCRATCH_DMA_ACCESS_OFF);
    result & FW_CFG_DMA_CTL_ERROR == 0
}

/// Reads `length` bytes of the item selected by `key` into `scratch + DIR_OFF`.
fn fw_cfg_read(
    fw_vaddr: u64,
    scratch_vaddr: u64,
    scratch_paddr: u64,
    key: u16,
    length: u32,
) -> bool {
    let control = (u32::from(key) << 16) | FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_READ;
    fw_cfg_dma(
        fw_vaddr,
        scratch_vaddr,
        scratch_paddr,
        control,
        length,
        scratch_paddr + SCRATCH_DIR_OFF,
    )
}

/// Writes the 28-byte config already encoded at `scratch + CONFIG_OFF` to the
/// item selected by `key`.
fn fw_cfg_write(
    fw_vaddr: u64,
    scratch_vaddr: u64,
    scratch_paddr: u64,
    key: u16,
    length: u32,
) -> bool {
    let control = (u32::from(key) << 16) | FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_WRITE;
    fw_cfg_dma(
        fw_vaddr,
        scratch_vaddr,
        scratch_paddr,
        control,
        length,
        scratch_paddr + SCRATCH_CONFIG_OFF,
    )
}

/// Scans the fw_cfg file directory for `etc/ramfb` and returns its selector.
fn find_ramfb_selector(fw_vaddr: u64, scratch_vaddr: u64, scratch_paddr: u64) -> Option<u16> {
    // First read just the 4-byte entry count.
    if !fw_cfg_read(fw_vaddr, scratch_vaddr, scratch_paddr, FW_CFG_FILE_DIR_KEY, 4) {
        return None;
    }
    let count = read_be32(scratch_vaddr + SCRATCH_DIR_OFF);
    if count == 0 || u64::from(count) > SCRATCH_DIR_CAPACITY / u64::from(FW_CFG_FILE_ENTRY_SIZE) {
        return None;
    }

    // Now read the whole directory: count + count * 64-byte entries.
    let total = 4 + count * FW_CFG_FILE_ENTRY_SIZE;
    if !fw_cfg_read(fw_vaddr, scratch_vaddr, scratch_paddr, FW_CFG_FILE_DIR_KEY, total) {
        return None;
    }

    let directory = scratch_vaddr + SCRATCH_DIR_OFF;
    for index in 0..count {
        let entry = directory + 4 + u64::from(index) * u64::from(FW_CFG_FILE_ENTRY_SIZE);
        if name_equals(entry + 8, b"etc/ramfb") {
            return Some(read_be16(entry + 4));
        }
    }
    None
}

/// Writes the RAMFBCfg descriptor and pushes it to `etc/ramfb`.
fn configure_ramfb(
    fw_vaddr: u64,
    scratch_vaddr: u64,
    scratch_paddr: u64,
    selector: u16,
    fb_paddr: u64,
) {
    // RAMFBCfg (all fields big-endian):
    //   addr (u64), fourcc (u32), flags (u32), width (u32), height (u32),
    //   stride (u32).
    let config = scratch_vaddr + SCRATCH_CONFIG_OFF;
    write_be64(config, fb_paddr);
    write_be32(config + 8, FOURCC_ARGB8888);
    write_be32(config + 12, 0); // flags
    write_be32(config + 16, SCREEN_WIDTH);
    write_be32(config + 20, SCREEN_HEIGHT);
    write_be32(config + 24, SCREEN_WIDTH * BYTES_PER_PIXEL); // stride (bytes)

    unsafe { core::arch::asm!("dmb sy", options(nostack)) };
    if !fw_cfg_write(fw_vaddr, scratch_vaddr, scratch_paddr, selector, 28) {
        syscall::print("GPU DRIVER: RAMFB CONFIG WRITE REJECTED\r\n");
    }
}

/// Compares a 56-byte NUL-padded name field against an ASCII literal.
fn name_equals(name_addr: u64, expected: &[u8]) -> bool {
    for (index, &byte) in expected.iter().enumerate() {
        let actual = read_u8(name_addr + index as u64);
        if actual != byte {
            return false;
        }
    }
    // The name must terminate with NUL at `expected.len()`.
    read_u8(name_addr + expected.len() as u64) == 0
}

// ---------------------------------------------------------------------------
// Big-endian helpers
// ---------------------------------------------------------------------------

fn read_u8(addr: u64) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

fn write_u8(addr: u64, value: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, value) };
}

fn read_be16(addr: u64) -> u16 {
    (u16::from(read_u8(addr)) << 8) | u16::from(read_u8(addr + 1))
}

fn read_be32(addr: u64) -> u32 {
    (u32::from(read_be16(addr)) << 16) | u32::from(read_be16(addr + 2))
}

fn write_be32(addr: u64, value: u32) {
    write_u8(addr, (value >> 24) as u8);
    write_u8(addr + 1, (value >> 16) as u8);
    write_u8(addr + 2, (value >> 8) as u8);
    write_u8(addr + 3, value as u8);
}

fn write_be64(addr: u64, value: u64) {
    write_be32(addr, (value >> 32) as u32);
    write_be32(addr + 4, value as u32);
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Fills the whole screen with `color`.
fn clear_screen(color: u32) {
    let Some(framebuffer) = (unsafe { *core::ptr::addr_of!(FRAMEBUFFER) }) else {
        return;
    };
    for py in 0..framebuffer.height {
        for px in 0..framebuffer.width {
            plot(framebuffer, px, py, color);
        }
    }
}

/// Draws a rectangle with a 1-pixel border. The interior is `fill_color` and
/// the perimeter is `border_color`.
fn draw_rect(x: u32, y: u32, w: u32, h: u32, fill_color: u32, border_color: u32) {
    let Some(framebuffer) = (unsafe { *core::ptr::addr_of!(FRAMEBUFFER) }) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    for py in y..y.saturating_add(h) {
        for px in x..x.saturating_add(w) {
            let border = px == x || px == x + w - 1 || py == y || py == y + h - 1;
            plot(framebuffer, px, py, if border { border_color } else { fill_color });
        }
    }
}

/// Draws `text` using the built-in 8x8 font. Characters outside the printable
/// ASCII range are rendered as spaces.
fn draw_text(x: u32, y: u32, text: &str, color: u32) {
    let mut cursor = x;
    for byte in text.bytes() {
        draw_char(cursor, y, byte, color);
        cursor = cursor.saturating_add(8);
    }
}

/// Draws `text` horizontally centered on `center_x`.
fn draw_text_centered(center_x: u32, y: u32, text: &str, color: u32) {
    let width = (text.len() as u32).saturating_mul(8);
    let x = center_x.saturating_sub(width / 2);
    draw_text(x, y, text, color);
}

fn draw_char(x: u32, y: u32, byte: u8, color: u32) {
    let Some(framebuffer) = (unsafe { *core::ptr::addr_of!(FRAMEBUFFER) }) else {
        return;
    };
    let index = match byte {
        0x20..=0x7E => (byte - 0x20) as usize,
        _ => 0, // space
    };
    for row in 0..8_u32 {
        let bits = FONT[index * 8 + row as usize];
        for col in 0..8_u32 {
            // Bit 0 is the leftmost pixel.
            if bits & (1_u8 << col) != 0 {
                plot(framebuffer, x + col, y + row, color);
            }
        }
    }
}

fn plot(framebuffer: Framebuffer, x: u32, y: u32, color: u32) {
    if x >= framebuffer.width || y >= framebuffer.height {
        return;
    }
    let offset = (u64::from(y) * u64::from(framebuffer.width) + u64::from(x))
        * u64::from(BYTES_PER_PIXEL);
    unsafe {
        core::ptr::write_volatile((framebuffer.vaddr + offset) as *mut u32, color);
    }
}

// ---------------------------------------------------------------------------
// Engineer menu
// ---------------------------------------------------------------------------

/// Draws the full engineer menu (cells, buttons, panel) without clearing the
/// frame or drawing the cursor. `pressed_btn` selects the pressed button fill.
fn draw_menu(pressed_btn: Option<usize>) {
    // Top: three small grey status cells.
    const CELL_Y: u32 = 20;
    const CELL_H: u32 = 44;
    const CELL_W: u32 = 192;
    const GAP: u32 = 16;
    const MARGIN: u32 = 16;

    let cells: [(&str, u32); 3] = [("Num", 0), ("No Data", 1), ("Date", 2)];
    for (label, index) in cells {
        let x = MARGIN + index * (CELL_W + GAP);
        draw_rect(x, CELL_Y, CELL_W, CELL_H, COLOR_GRAY, COLOR_LIGHT_GRAY);
        draw_text_centered(x + CELL_W / 2, CELL_Y + CELL_H / 2 - 4, label, COLOR_BLACK);
    }

    // Center: 3 x 2 grid of blue buttons with white text.
    for index in 0..BUTTON_LABELS.len() {
        draw_button(index, pressed_btn == Some(index));
    }

    // Bottom: one wide grey panel with serial and OS version.
    const PANEL_Y: u32 = 340;
    const PANEL_H: u32 = 100;
    draw_rect(
        MARGIN,
        PANEL_Y,
        SCREEN_WIDTH - 2 * MARGIN,
        PANEL_H,
        COLOR_GRAY,
        COLOR_LIGHT_GRAY,
    );
    draw_text_centered(SCREEN_WIDTH / 2, PANEL_Y + 24, "SN: 877...", COLOR_BLACK);
    draw_text_centered(SCREEN_WIDTH / 2, PANEL_Y + 56, "Silver Microkernel v0.1", COLOR_BLACK);
}

/// Repaints the whole frame: clear, menu, then the cursor on top.
fn render(cursor_x: u16, cursor_y: u16, pressed_btn: Option<usize>) {
    clear_screen(COLOR_BLACK);
    draw_menu(pressed_btn);
    draw_cursor(cursor_x, cursor_y);
}

/// Returns the pixel rectangle of button `index` (row-major, 3 columns).
fn button_rect(index: usize) -> (u32, u32, u32, u32) {
    let col = (index % 3) as u32;
    let row = (index / 3) as u32;
    let x = BUTTON_GRID_X + col * (BUTTON_W + BUTTON_GAP);
    let y = BUTTON_GRID_Y + row * (BUTTON_H + BUTTON_GAP);
    (x, y, BUTTON_W, BUTTON_H)
}

/// Draws one button, dark blue while `pressed` and the normal blue otherwise.
fn draw_button(index: usize, pressed: bool) {
    let (x, y, w, h) = button_rect(index);
    let fill = if pressed { COLOR_PRESSED_BLUE } else { COLOR_BLUE };
    draw_rect(x, y, w, h, fill, COLOR_DARK_GRAY);
    draw_text_centered(x + w / 2, y + h / 2 - 4, BUTTON_LABELS[index], COLOR_WHITE);
}

/// Returns the index of the button under `(x, y)`, if any.
fn button_at(x: u16, y: u16) -> Option<usize> {
    for index in 0..BUTTON_LABELS.len() {
        let (bx, by, bw, bh) = button_rect(index);
        let within_x = u32::from(x) >= bx && u32::from(x) < bx + bw;
        let within_y = u32::from(y) >= by && u32::from(y) < by + bh;
        if within_x && within_y {
            return Some(index);
        }
    }
    None
}

/// Draws a small white arrow pointer with its tip at `(x, y)`.
fn draw_cursor(x: u16, y: u16) {
    // 8x8 arrow: bit 7 of each row byte is the leftmost pixel, so row 0 is the
    // tip and each row grows one pixel to the right.
    const CURSOR: [u8; 8] = [
        0b1000_0000,
        0b1100_0000,
        0b1110_0000,
        0b1111_0000,
        0b1111_1000,
        0b1111_1100,
        0b1111_1110,
        0b1111_1111,
    ];
    let Some(framebuffer) = (unsafe { *core::ptr::addr_of!(FRAMEBUFFER) }) else {
        return;
    };
    for row in 0..8_u32 {
        let bits = CURSOR[row as usize];
        for col in 0..8_u32 {
            if bits & (1_u8 << (7 - col)) != 0 {
                plot(framebuffer, u32::from(x) + col, u32::from(y) + row, COLOR_WHITE);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8x8 public-domain font (font8x8_basic), ASCII 0x20..=0x7E.
// Bit 0 of each row byte is the leftmost pixel.
// ---------------------------------------------------------------------------

#[allow(clippy::unreadable_literal)]
static FONT: [u8; 95 * 8] = [
    // 0x20 ' '
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x21 '!'
    0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00,
    // 0x22 '"'
    0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x23 '#'
    0x36, 0x36, 0x7F, 0x36, 0x7F, 0x36, 0x36, 0x00,
    // 0x24 '$'
    0x0C, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x0C, 0x00,
    // 0x25 '%'
    0x00, 0x63, 0x33, 0x18, 0x0C, 0x66, 0x63, 0x00,
    // 0x26 '&'
    0x1C, 0x36, 0x1C, 0x6E, 0x3B, 0x33, 0x6E, 0x00,
    // 0x27 '\''
    0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x28 '('
    0x18, 0x0C, 0x06, 0x06, 0x06, 0x0C, 0x18, 0x00,
    // 0x29 ')'
    0x06, 0x0C, 0x18, 0x18, 0x18, 0x0C, 0x06, 0x00,
    // 0x2A '*'
    0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00,
    // 0x2B '+'
    0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00,
    // 0x2C ','
    0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x06,
    // 0x2D '-'
    0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0x00,
    // 0x2E '.'
    0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x00,
    // 0x2F '/'
    0x60, 0x30, 0x18, 0x0C, 0x06, 0x03, 0x01, 0x00,
    // 0x30 '0'
    0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00,
    // 0x31 '1'
    0x0C, 0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x3F, 0x00,
    // 0x32 '2'
    0x1E, 0x33, 0x30, 0x1C, 0x06, 0x33, 0x3F, 0x00,
    // 0x33 '3'
    0x1E, 0x33, 0x30, 0x1C, 0x30, 0x33, 0x1E, 0x00,
    // 0x34 '4'
    0x38, 0x3C, 0x36, 0x33, 0x7F, 0x30, 0x78, 0x00,
    // 0x35 '5'
    0x3F, 0x03, 0x1F, 0x30, 0x30, 0x33, 0x1E, 0x00,
    // 0x36 '6'
    0x1C, 0x06, 0x03, 0x1F, 0x33, 0x33, 0x1E, 0x00,
    // 0x37 '7'
    0x3F, 0x33, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x00,
    // 0x38 '8'
    0x1E, 0x33, 0x33, 0x1E, 0x33, 0x33, 0x1E, 0x00,
    // 0x39 '9'
    0x1E, 0x33, 0x33, 0x3E, 0x30, 0x18, 0x0E, 0x00,
    // 0x3A ':'
    0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x00,
    // 0x3B ';'
    0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x06,
    // 0x3C '<'
    0x18, 0x0C, 0x06, 0x03, 0x06, 0x0C, 0x18, 0x00,
    // 0x3D '='
    0x00, 0x00, 0x3F, 0x00, 0x00, 0x3F, 0x00, 0x00,
    // 0x3E '>'
    0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00,
    // 0x3F '?'
    0x1E, 0x33, 0x30, 0x18, 0x0C, 0x00, 0x0C, 0x00,
    // 0x40 '@'
    0x3E, 0x63, 0x7B, 0x7B, 0x7B, 0x03, 0x1E, 0x00,
    // 0x41 'A'
    0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00,
    // 0x42 'B'
    0x3F, 0x66, 0x66, 0x3E, 0x66, 0x66, 0x3F, 0x00,
    // 0x43 'C'
    0x3C, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3C, 0x00,
    // 0x44 'D'
    0x1F, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1F, 0x00,
    // 0x45 'E'
    0x7F, 0x46, 0x16, 0x1E, 0x16, 0x46, 0x7F, 0x00,
    // 0x46 'F'
    0x7F, 0x46, 0x16, 0x1E, 0x16, 0x06, 0x0F, 0x00,
    // 0x47 'G'
    0x3C, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7C, 0x00,
    // 0x48 'H'
    0x33, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x33, 0x00,
    // 0x49 'I'
    0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00,
    // 0x4A 'J'
    0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E, 0x00,
    // 0x4B 'K'
    0x67, 0x66, 0x36, 0x1E, 0x36, 0x66, 0x67, 0x00,
    // 0x4C 'L'
    0x0F, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7F, 0x00,
    // 0x4D 'M'
    0x63, 0x77, 0x7F, 0x7F, 0x6B, 0x63, 0x63, 0x00,
    // 0x4E 'N'
    0x63, 0x67, 0x6F, 0x7B, 0x73, 0x63, 0x63, 0x00,
    // 0x4F 'O'
    0x1C, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1C, 0x00,
    // 0x50 'P'
    0x3F, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x0F, 0x00,
    // 0x51 'Q'
    0x1E, 0x33, 0x33, 0x33, 0x3B, 0x1E, 0x38, 0x00,
    // 0x52 'R'
    0x3F, 0x66, 0x66, 0x3E, 0x36, 0x66, 0x67, 0x00,
    // 0x53 'S'
    0x1E, 0x33, 0x07, 0x0E, 0x38, 0x33, 0x1E, 0x00,
    // 0x54 'T'
    0x3F, 0x2D, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00,
    // 0x55 'U'
    0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3F, 0x00,
    // 0x56 'V'
    0x33, 0x33, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00,
    // 0x57 'W'
    0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00,
    // 0x58 'X'
    0x63, 0x63, 0x36, 0x1C, 0x1C, 0x36, 0x63, 0x00,
    // 0x59 'Y'
    0x33, 0x33, 0x33, 0x1E, 0x0C, 0x0C, 0x1E, 0x00,
    // 0x5A 'Z'
    0x7F, 0x63, 0x31, 0x18, 0x4C, 0x66, 0x7F, 0x00,
    // 0x5B '['
    0x1E, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1E, 0x00,
    // 0x5C '\\'
    0x03, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00,
    // 0x5D ']'
    0x1E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1E, 0x00,
    // 0x5E '^'
    0x08, 0x1C, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00,
    // 0x5F '_'
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF,
    // 0x60 '`'
    0x0C, 0x0C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x61 'a'
    0x00, 0x00, 0x1E, 0x30, 0x3E, 0x33, 0x6E, 0x00,
    // 0x62 'b'
    0x07, 0x06, 0x06, 0x3E, 0x66, 0x66, 0x3B, 0x00,
    // 0x63 'c'
    0x00, 0x00, 0x1E, 0x33, 0x03, 0x33, 0x1E, 0x00,
    // 0x64 'd'
    0x38, 0x30, 0x30, 0x3E, 0x33, 0x33, 0x6E, 0x00,
    // 0x65 'e'
    0x00, 0x00, 0x1E, 0x33, 0x3F, 0x03, 0x1E, 0x00,
    // 0x66 'f'
    0x1C, 0x36, 0x06, 0x0F, 0x06, 0x06, 0x0F, 0x00,
    // 0x67 'g'
    0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x1F,
    // 0x68 'h'
    0x07, 0x06, 0x36, 0x6E, 0x66, 0x66, 0x67, 0x00,
    // 0x69 'i'
    0x0C, 0x00, 0x0E, 0x0C, 0x0C, 0x0C, 0x1E, 0x00,
    // 0x6A 'j'
    0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E,
    // 0x6B 'k'
    0x07, 0x06, 0x66, 0x36, 0x1E, 0x36, 0x67, 0x00,
    // 0x6C 'l'
    0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00,
    // 0x6D 'm'
    0x00, 0x00, 0x33, 0x7F, 0x7F, 0x6B, 0x63, 0x00,
    // 0x6E 'n'
    0x00, 0x00, 0x1F, 0x33, 0x33, 0x33, 0x33, 0x00,
    // 0x6F 'o'
    0x00, 0x00, 0x1E, 0x33, 0x33, 0x33, 0x1E, 0x00,
    // 0x70 'p'
    0x00, 0x00, 0x3B, 0x66, 0x66, 0x3E, 0x06, 0x0F,
    // 0x71 'q'
    0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x78,
    // 0x72 'r'
    0x00, 0x00, 0x3B, 0x6E, 0x66, 0x06, 0x0F, 0x00,
    // 0x73 's'
    0x00, 0x00, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x00,
    // 0x74 't'
    0x08, 0x0C, 0x3E, 0x0C, 0x0C, 0x2C, 0x18, 0x00,
    // 0x75 'u'
    0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6E, 0x00,
    // 0x76 'v'
    0x00, 0x00, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00,
    // 0x77 'w'
    0x00, 0x00, 0x63, 0x6B, 0x7F, 0x7F, 0x36, 0x00,
    // 0x78 'x'
    0x00, 0x00, 0x63, 0x36, 0x1C, 0x36, 0x63, 0x00,
    // 0x79 'y'
    0x00, 0x00, 0x33, 0x33, 0x33, 0x3E, 0x30, 0x1F,
    // 0x7A 'z'
    0x00, 0x00, 0x3F, 0x19, 0x0C, 0x26, 0x3F, 0x00,
    // 0x7B '{'
    0x38, 0x0C, 0x0C, 0x07, 0x0C, 0x0C, 0x38, 0x00,
    // 0x7C '|'
    0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00,
    // 0x7D '}'
    0x07, 0x0C, 0x0C, 0x38, 0x0C, 0x0C, 0x07, 0x00,
    // 0x7E '~'
    0x6E, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn fail(message: &str) -> ! {
    syscall::print(message);
    syscall::exit_with_status(1)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
