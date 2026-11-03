//! Read-only GICv3 / Generic Timer register dump for UART-less bring-up.
//!
//! The Rain unit has no accessible UART, so the only debug channel is the
//! `cont_splash` framebuffer. This module samples architectural state at four
//! points and paints the raw values as hexadecimal text.
//!
//! Strict rules, because a wrong write here would invalidate the experiment:
//!
//! * only `MRS` and MMIO reads are used;
//! * `ICC_IAR1_EL1` is never read outside the real interrupt handler (a read
//!   would acknowledge the interrupt);
//! * no GIC, timer, security or DPG bit is written;
//! * `HCR_EL2` is read only when `CurrentEL` is EL2, because at EL1 the read
//!   traps and would turn the diagnostic into a fault.
//!
//! The output is three large pages, cycled automatically because the unit has
//! no input device the kernel can read yet:
//!
//! * `A` — static facts (GIC bases, FDT cells, INTID and its source);
//! * `B` — sample 1 (before the timer is armed) and sample 2 (after);
//! * `C` — sample 3 (before the wait) and sample 4 (after a delivered IRQ).

use core::sync::atomic::{AtomicU64, Ordering};

macro_rules! read_sysreg {
    ($register:literal) => {{
        let value: u64;
        unsafe {
            core::arch::asm!(
                concat!("mrs {}, ", $register),
                out(reg) value,
                options(nomem, nostack),
            );
        }
        value
    }};
}

/// Sampling points: before the timer is armed, after it is armed, before the
/// idle wait, and after an interrupt was actually observed.
pub const STAGES: usize = 4;
/// One entry per row in [`DYNAMIC_CODES`].
const DYNAMIC_ROWS: usize = 23;
const SAMPLE_WORDS: usize = STAGES * DYNAMIC_ROWS;

static SAMPLES: [AtomicU64; SAMPLE_WORDS] = [const { AtomicU64::new(0) }; SAMPLE_WORDS];
static CAPTURED: [AtomicU64; STAGES] = [const { AtomicU64::new(0) }; STAGES];
static WAKE_SEEN: AtomicU64 = AtomicU64::new(0);

// --- framebuffer geometry -------------------------------------------------
// Rain/Fog `cont_splash_region@5c000000`; the panel is 720x1650 and the splash
// buffer is scanned out as 32-bit ARGB with a 720-pixel stride.
const FB_BASE: usize = 0x5C00_0000;
const FB_WIDTH: usize = 720;
const FB_HEIGHT: usize = 1650;
const FB_STRIDE: usize = 720;

const BLACK: u32 = 0xFF00_0000;
const WHITE: u32 = 0xFFFF_FFFF;
const GREY: u32 = 0xFF40_4040;
const ORANGE: u32 = 0xFFFF_8000;
const YELLOW: u32 = 0xFFFF_FF00;
const CYAN: u32 = 0xFF00_FFFF;
const GREEN: u32 = 0xFF00_FF00;

/// How long one page stays on screen. Long enough to take a steady photo.
const PAGE_SECONDS: u64 = 15;
const PAGES: usize = 1;

/// Marker + space + two-digit row code + space + 16 hex digits.
const MAX_CHARS: usize = 21;
const MAX_LINES: usize = 64;

const STAGE1: u8 = 1;
const STAGE2: u8 = 2;
const STAGE3: u8 = 3;
const STAGE4: u8 = 4;

/// Row codes of one dynamic sample, in display order.
const DYNAMIC_CODES: [u8; DYNAMIC_ROWS] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
];

/// Number of hex digits per dynamic row (leading zeros are printed).
const DYNAMIC_DIGITS: [u8; DYNAMIC_ROWS] = [
    16, // 01 CurrentEL
    16, // 02 MPIDR_EL1
    16, // 03 DAIF
    16, // 04 VBAR_EL1
    16, // 05 CNTP_CTL_EL0
    16, // 06 CNTP_TVAL_EL0
    16, // 07 CNTP_CVAL_EL0
    16, // 08 CNTFRQ_EL0
    16, // 09 CNTPCT_EL0
    8,  // 0A GICD_CTLR
    8,  // 0B GICR_WAKER
    8,  // 0C GICR_CTLR
    8,  // 0D GICR_IGROUPR0
    8,  // 0E GICR_IGRPMODR0
    8,  // 0F GICR_ISENABLER0
    8,  // 10 GICR_ISPENDR0
    8,  // 11 GICR_ICFGR1
    8,  // 12 ICC_SRE_EL1
    8,  // 13 ICC_CTLR_EL1
    8,  // 14 ICC_PMR_EL1
    8,  // 15 ICC_IGRPEN1_EL1
    8,  // 16 ICC_HPPIR1_EL1
    8,  // 17 ICC_RPR_EL1
];

/// Captures one sample. `stage` is 0..3.
pub fn capture(stage: usize) {
    if stage >= STAGES {
        return;
    }
    let gic = crate::gicv3::diag_gic();
    let mut values = [0u64; DYNAMIC_ROWS];

    values[0] = read_sysreg!("currentel");
    values[1] = read_sysreg!("mpidr_el1");
    values[2] = read_sysreg!("daif");
    values[3] = read_sysreg!("vbar_el1");

    values[4] = read_sysreg!("cntp_ctl_el0");
    values[5] = read_sysreg!("cntp_tval_el0");
    values[6] = read_sysreg!("cntp_cval_el0");
    values[7] = read_sysreg!("cntfrq_el0");
    values[8] = read_sysreg!("cntpct_el0");

    values[9] = u64::from(gic.gicd_ctlr);
    values[10] = u64::from(gic.gicr_waker);
    values[11] = u64::from(gic.gicr_ctlr);
    values[12] = u64::from(gic.gicr_igroupr0);
    values[13] = u64::from(gic.gicr_igrpmodr0);
    values[14] = u64::from(gic.gicr_isenabler0);
    values[15] = u64::from(gic.gicr_ispendr0);
    values[16] = u64::from(gic.gicr_icfgr1);

    values[17] = read_sysreg!("icc_sre_el1");
    values[18] = read_sysreg!("icc_ctlr_el1");
    values[19] = read_sysreg!("icc_pmr_el1");
    values[20] = read_sysreg!("icc_igrpen1_el1");
    values[21] = read_sysreg!("icc_hppir1_el1");
    values[22] = read_sysreg!("icc_rpr_el1");

    let base = stage * DYNAMIC_ROWS;
    for (index, value) in values.iter().enumerate() {
        SAMPLES[base + index].store(*value, Ordering::Relaxed);
    }
    CAPTURED[stage].store(1, Ordering::Relaxed);
}

/// Runs the paged display forever. Called from the idle task instead of the
/// `WFI` loop only in `fog-diag` builds: the unit has no input device, so the
/// operator needs time to photograph each page.
pub fn run_display() -> ! {
    let frequency = {
        let value = read_sysreg!("cntfrq_el0");
        if value == 0 {
            19_200_000
        } else {
            value
        }
    };
    let dwell = frequency.saturating_mul(PAGE_SECONDS);
    let mut page = 0usize;

    loop {
        render_page(page);
        let start = read_sysreg!("cntpct_el0");
        let mut last_mark = u64::MAX;

        loop {
            let elapsed = read_sysreg!("cntpct_el0").wrapping_sub(start);
            if elapsed >= dwell {
                break;
            }
            let mark = elapsed / (frequency / 50).max(1);
            if mark != last_mark {
                last_mark = mark;
                draw_progress(elapsed, dwell);
            }
            // If the timer ever reaches the CPU, record the post-interrupt
            // sample exactly once. `timer::ticks` is bumped by the real
            // handler, so this cannot fire spuriously.
            if WAKE_SEEN.load(Ordering::Relaxed) == 0 && crate::timer::ticks() > 0 {
                WAKE_SEEN.store(1, Ordering::Relaxed);
                capture(3);
            }
            core::hint::spin_loop();
        }
        page = (page + 1) % PAGES;
    }
}

#[derive(Clone, Copy)]
struct Line {
    marker: u8,
    code: u8,
    value: u64,
    digits: u8,
}

impl Line {
    const EMPTY: Self = Self {
        marker: 0,
        code: 0,
        value: 0,
        digits: 8,
    };
}

fn render_page(page: usize) {
    clear();
    border();
    let mut lines = [Line::EMPTY; MAX_LINES];

    match page {
        0 => {
            draw_char(20, 16, b'A', WHITE, 6);
            let count = build_static(&mut lines);
            for (index, line) in lines[..count].iter().enumerate() {
                draw_line(24, 100 + index * 38, line, 4);
            }
        }
        1 => {
            draw_char(20, 16, b'B', WHITE, 6);
            let count = build_dynamic(0, &mut lines, 0);
            let count = build_dynamic(1, &mut lines, count);
            for index in 0..count {
                let gap = if index >= DYNAMIC_ROWS { 8 } else { 0 };
                draw_line(24, 90 + index * 32 + gap, &lines[index], 3);
            }
        }
        _ => {
            draw_char(20, 16, b'C', WHITE, 6);
            let count = build_dynamic(2, &mut lines, 0);
            let count = build_dynamic(3, &mut lines, count);
            for index in 0..count {
                let gap = if index >= DYNAMIC_ROWS { 8 } else { 0 };
                draw_line(24, 90 + index * 32 + gap, &lines[index], 3);
            }
        }
    }
    draw_progress(0, 1);
}

fn build_static(lines: &mut [Line; MAX_LINES]) -> usize {
    let info = crate::fog::info();
    let gic = crate::gicv3::diag_gic();
    let mpidr = read_sysreg!("mpidr_el1");
    let affinity = crate::gicv3_policy::mpidr_affinity(mpidr);
    let rd_affinity = crate::gicv3_policy::typer_affinity(gic.gicr_typer);
    let (hcr_available, _hcr) = read_hcr_el2();

    let mut n = 0;
    let mut push = |code: u8, value: u64, digits: u8| {
        if n < MAX_LINES {
            lines[n] = Line {
                marker: 0,
                // Static codes live in 0x41.. so they cannot be confused with
                // the per-stage 0x01..0x17 dynamic rows.
                code: code.wrapping_add(0x40),
                value,
                digits,
            };
            n += 1;
        }
    };

    push(0x01, gic.distributor as u64, 16);
    push(0x02, gic.redistributor as u64, 16);
    push(0x03, gic.stride as u64, 8);
    push(0x04, gic.frames as u64, 8);
    push(0x05, u64::from(gic.frame_index), 8);
    push(0x06, mpidr, 16);
    push(0x07, affinity, 8);
    push(0x08, gic.gicr_typer, 16);
    push(0x09, rd_affinity, 8);
    push(0x0A, u64::from(info.timer_irq), 8);
    push(0x0B, if info.timer_from_fdt { 1 } else { 2 }, 8);
    push(0x0C, u64::from(gic.gicd_iidr), 8);
    push(0x0D, u64::from(gic.gicr_iidr), 8);

    push(0x0E, u64::from(info.gic_node as u8), 8);
    push(0x0F, info.gic_cells_len as u64, 8);
    for cell in 0..4 {
        push(
            0x10 + cell as u8,
            u64::from(info.gic_cells.get(cell).copied().unwrap_or(0)),
            8,
        );
    }

    push(0x14, u64::from(info.timer_node as u8), 8);
    push(0x15, info.timer_cells_len as u64, 8);
    for cell in 0..6 {
        push(
            0x16 + cell as u8,
            u64::from(info.timer_cells.get(cell).copied().unwrap_or(0)),
            8,
        );
    }
    push(0x1C, if hcr_available { 1 } else { 0 }, 8);
    push(0x1D, u64::from(gic.gicd_typer), 8);
    push(0x1E, u64::from(gic.gicr_icfgr0), 8);
    push(0x1F, u64::from(gic.gicr_ipriorityr0), 8);
    push(0x20, u64::from(gic.gicd_statusr), 8);
    push(0x21, u64::from(gic.gicr_sgi_priorityr), 8);
    // Live readback, taken when the page is painted (after gic::init):
    // 62/63/64 show whether ARE_NS and the PPI enable stuck, 67 counts real
    // timer interrupts.
    push(0x22, u64::from(gic.gicd_ctlr), 8);
    push(0x23, u64::from(gic.gicr_isenabler0), 8);
    push(0x24, u64::from(gic.gicr_ispendr0), 8);
    push(0x25, read_sysreg!("cntp_ctl_el0"), 16);
    push(0x26, read_sysreg!("icc_hppir1_el1"), 8);
    push(0x27, crate::timer::ticks(), 16);

    drop(push);
    n
}

fn build_dynamic(stage: usize, lines: &mut [Line], start: usize) -> usize {
    let marker = if CAPTURED[stage].load(Ordering::Relaxed) != 0 {
        (stage + 1) as u8
    } else {
        0x0F
    };
    let base = stage * DYNAMIC_ROWS;
    let mut n = start;
    for row in 0..DYNAMIC_ROWS {
        let value = if marker == 0x0F {
            u64::MAX
        } else {
            SAMPLES[base + row].load(Ordering::Relaxed)
        };
        lines[n] = Line {
            marker,
            code: DYNAMIC_CODES[row],
            value,
            digits: DYNAMIC_DIGITS[row],
        };
        n += 1;
    }
    n
}

/// `HCR_EL2` is only architecturally accessible at EL2 (or EL1 with E2H).
/// Reading it at plain EL1 traps, so the availability is reported instead.
fn read_hcr_el2() -> (bool, u64) {
    let current_el = read_sysreg!("currentel") >> 2;
    if current_el >= 2 {
        (true, read_sysreg!("hcr_el2"))
    } else {
        (false, 0)
    }
}

// --- framebuffer ----------------------------------------------------------

fn clear() {
    let base = crate::memory::phys_to_virt(FB_BASE) as *mut u32;
    for row in 0..FB_HEIGHT {
        let line = unsafe { base.add(row * FB_STRIDE) };
        for column in 0..FB_WIDTH {
            unsafe { line.add(column).write_volatile(BLACK) };
        }
    }
}

/// A thin frame makes a wrong stride visible.
fn border() {
    let base = crate::memory::phys_to_virt(FB_BASE) as *mut u32;
    for x in 0..FB_WIDTH {
        unsafe {
            base.add(2 * FB_STRIDE + x).write_volatile(WHITE);
            base.add((FB_HEIGHT - 3) * FB_STRIDE + x).write_volatile(WHITE);
        }
    }
    for y in 0..FB_HEIGHT {
        unsafe {
            base.add(y * FB_STRIDE + 2).write_volatile(WHITE);
            base.add(y * FB_STRIDE + FB_WIDTH - 3).write_volatile(WHITE);
        }
    }
}

fn draw_progress(elapsed: u64, dwell: u64) {
    const X: usize = 24;
    const Y: usize = 1604;
    const W: usize = 672;
    const H: usize = 8;
    fill_rect(X, Y, W, H, GREY);
    let ratio = if dwell == 0 {
        0
    } else {
        (u128::from(elapsed.min(dwell)) * W as u128 / u128::from(dwell)) as usize
    };
    fill_rect(X, Y, ratio, H, WHITE);
}

fn draw_line(x: usize, y: usize, line: &Line, scale: usize) {
    let marker_color = match line.marker {
        STAGE1 => ORANGE,
        STAGE2 => YELLOW,
        STAGE3 => CYAN,
        STAGE4 => GREEN,
        0 => WHITE,
        _ => GREY,
    };
    let char_w = 8 * scale;
    let char_h = 8 * scale;
    fill_rect(x.saturating_sub(8), y, 5, char_h, marker_color);

    let mut buffer = [b' '; MAX_CHARS];
    buffer[0] = hex_digit(line.marker);
    buffer[2] = hex_digit((line.code >> 4) & 0xF);
    buffer[3] = hex_digit(line.code & 0xF);
    let digits = line.digits.min(16) as usize;
    for index in 0..digits {
        let shift = 4 * (digits - 1 - index) as u32;
        buffer[5 + index] = hex_digit(((line.value >> shift) & 0xF) as u8);
    }

    let color = if line.marker == 0x0F { GREY } else { WHITE };
    for (index, byte) in buffer.iter().enumerate() {
        draw_char(x + index * char_w, y, *byte, color, scale);
    }
}

fn hex_digit(value: u8) -> u8 {
    match value & 0xF {
        0..=9 => b'0' + (value & 0xF),
        _ => b'A' + ((value & 0xF) - 10),
    }
}

fn draw_char(x: usize, y: usize, character: u8, color: u32, scale: usize) {
    let glyph = glyph(character);
    for (row, bits) in glyph.iter().enumerate() {
        for column in 0..8 {
            if bits & (0x80 >> column) != 0 {
                fill_rect(x + column * scale, y + row * scale, scale, scale, color);
            }
        }
    }
}

fn fill_rect(x: usize, y: usize, w: usize, h: usize, color: u32) {
    if x >= FB_WIDTH || y >= FB_HEIGHT {
        return;
    }
    let base = crate::memory::phys_to_virt(FB_BASE) as *mut u32;
    let width = w.min(FB_WIDTH - x);
    for row in 0..h {
        if y + row >= FB_HEIGHT {
            break;
        }
        let line = unsafe { base.add((y + row) * FB_STRIDE + x) };
        for column in 0..width {
            unsafe { line.add(column).write_volatile(color) };
        }
    }
}

/// 8x8 bitmap font subset: `0`-`9`, `A`-`F` and space. No other glyph is
/// needed because every label is a numeric row code plus the page letter.
fn glyph(character: u8) -> [u8; 8] {
    match character {
        b'0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
        b'1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        b'2' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x7E, 0x00],
        b'3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
        b'4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
        b'5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
        b'6' => [0x1C, 0x30, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
        b'7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
        b'8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
        b'9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x0C, 0x38, 0x00],
        b'A' => [0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x00],
        b'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
        b'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
        b'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
        b'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
        b'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
        _ => [0; 8],
    }
}
