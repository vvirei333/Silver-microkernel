//! Qualcomm GENI SE (Serial Engine) UART driver — polled TX/RX.
//!
//! SM8650 and SM6225 both expose a GENI UART, but at different addresses:
//! `0x0089_C000` (SE15, tablet) and `0x04A9_0000` (SE4, khaje). The base is
//! therefore configured at runtime from the live FDT; nothing here assumes a
//! board address. If no base could be proved, the driver stays disabled and
//! every access is a no-op instead of a bus abort.
//!
//! Unlike the PL011 on QEMU `virt`, the GENI UART is driven by a small state
//! machine: the main sequencer is handed a transfer length and a `START_TX`
//! opcode, the bytes are pushed into a word-based FIFO, and the sequencer
//! drains the FIFO to the line on its own. RX is the mirror image: the
//! secondary sequencer is started once with `START_READ` and fills the RX
//! FIFO; this driver drains that FIFO by polling.
//!
//! The register offsets and bit fields mirror the Linux `qcom_geni_serial.c`
//! / `geni-se.h` (the offsets are hardware, not API). The driver relies on
//! the bootloader having brought up the SE clock at the rate for 115200 8n1 —
//! re-programming the clock tree is out of scope for the first boot console.

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Physical base configured by `init_at`. Zero means "not proven, stay off".
static BASE: AtomicUsize = AtomicUsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);

/// Cached RX word and the number of valid bytes still in it. The GENI FIFO
/// packs four 8-bit characters per 32-bit word (LSB first).
static RX_WORD: AtomicU32 = AtomicU32::new(0);
static RX_COUNT: AtomicU32 = AtomicU32::new(0);

// --- SE register offsets (from Linux `geni-se.h`) ---------------------------
const SE_GENI_STATUS: usize = 0x40;
const SE_GENI_DMA_MODE_EN: usize = 0x258;
const SE_GENI_BYTE_GRAN: usize = 0x254;
const SE_GENI_TX_PACKING_CFG0: usize = 0x260;
const SE_GENI_TX_PACKING_CFG1: usize = 0x264;
const SE_UART_TX_WORD_LEN: usize = 0x268;
const SE_UART_TX_STOP_BIT_LEN: usize = 0x26c;
const SE_UART_TX_TRANS_LEN: usize = 0x270;
const SE_UART_TX_TRANS_CFG: usize = 0x25c;
const SE_UART_TX_PARITY_CFG: usize = 0x2a4;
const SE_UART_RX_TRANS_CFG: usize = 0x280;
const SE_GENI_RX_PACKING_CFG0: usize = 0x284;
const SE_GENI_RX_PACKING_CFG1: usize = 0x288;
const SE_UART_RX_WORD_LEN: usize = 0x28c;
const SE_UART_RX_STALE_CNT: usize = 0x294;
const SE_UART_RX_PARITY_CFG: usize = 0x2a8;
const SE_GENI_M_CMD0: usize = 0x600;
const SE_GENI_M_CMD_CTRL_REG: usize = 0x604;
const SE_GENI_M_IRQ_STATUS: usize = 0x610;
const SE_GENI_M_IRQ_EN: usize = 0x614;
const SE_GENI_M_IRQ_CLEAR: usize = 0x618;
const SE_GENI_S_CMD0: usize = 0x630;
const SE_GENI_S_CMD_CTRL_REG: usize = 0x634;
const SE_GENI_S_IRQ_STATUS: usize = 0x640;
const SE_GENI_S_IRQ_EN: usize = 0x644;
const SE_GENI_S_IRQ_CLEAR: usize = 0x648;
const SE_GENI_TX_FIFON: usize = 0x700;
const SE_GENI_RX_FIFON: usize = 0x780;
const SE_GENI_TX_FIFO_STATUS: usize = 0x800;
const SE_GENI_RX_FIFO_STATUS: usize = 0x804;
const SE_GENI_TX_WATERMARK_REG: usize = 0x80c;
const SE_GENI_RX_WATERMARK_REG: usize = 0x810;

// --- Bit fields (from Linux `geni-se.h` / `qcom_geni_serial.c`) ------------
/// `SE_GENI_STATUS`: main sequencer has a command in flight.
const M_GENI_CMD_ACTIVE: u32 = 1 << 0;
/// Opcode lives in the top 5 bits of `SE_GENI_M_CMD0` / `SE_GENI_S_CMD0`.
const M_OPCODE_SHFT: u32 = 27;
const S_OPCODE_SHFT: u32 = 27;
const OPCODE_MASK: u32 = 0x1F << 27;
const UART_START_TX: u32 = 0x1;
const UART_START_READ: u32 = 0x1;
/// `SE_GENI_M_IRQ_STATUS`: FIFO drained below the watermark (room to write).
const M_TX_FIFO_WATERMARK_EN: u32 = 1 << 30;
/// `SE_GENI_M_IRQ_STATUS`: the last START_TX transfer completed.
const M_CMD_DONE_EN: u32 = 1 << 0;
/// `SE_GENI_M_CMD_CTRL_REG`: abort a stuck transfer.
const M_GENI_CMD_ABORT: u32 = 1 << 1;
/// `SE_UART_TX_TRANS_CFG` bit 1: no hardware flow control.
const UART_CTS_MASK: u32 = 1 << 1;
/// `SE_GENI_DMA_MODE_EN` bit 0: keep FIFO (not DMA) mode.
const GENI_DMA_MODE_EN: u32 = 1 << 0;

/// 8-bit chars packed 4 to a FIFO word, LSB-first (bpw=8, pack_words=4,
/// `msb_to_lsb=false`), computed exactly as Linux `geni_se_config_packing` does.
const PACKING_CFG0_VALUE: u32 = 0x0004_380E;
const PACKING_CFG1_VALUE: u32 = 0x000C_5E0E;
/// `DEF_TX_WM`: fire the watermark IRQ when the FIFO has this many words left.
const TX_WATERMARK: u32 = 2;
const RX_WATERMARK: u32 = 2;
/// Linux `DEFAULT_BITS_PER_CHAR * STALE_TIMEOUT`.
const RX_STALE_COUNT: u32 = 10 * 16;

/// `SE_GENI_RX_FIFO_STATUS`: word count, `RX_LAST` and the number of valid
/// bytes in the last word.
const RX_FIFO_WC_MSK: u32 = 0x01FF_FFFF;
const RX_LAST: u32 = 1 << 31;
const RX_LAST_BYTE_VALID_MSK: u32 = 0x7 << 28;

/// Stub state for the Ctrl-Z "stop console" job-control hook. RX support now
/// exists, so the byte is recognised in `read_byte`.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Configures the UART at `base_phys`. A zero base leaves the driver disabled.
/// The SE clock is assumed to be already running at the 115200 sampling rate
/// (set up by the bootloader).
pub fn init_at(base_phys: usize) {
    if base_phys == 0 {
        READY.store(false, Ordering::Release);
        BASE.store(0, Ordering::Release);
        return;
    }
    BASE.store(crate::memory::phys_to_virt(base_phys), Ordering::Release);
    RX_WORD.store(0, Ordering::Relaxed);
    RX_COUNT.store(0, Ordering::Relaxed);

    // Clear every GENI interrupt so no stale status bit can fake a completion.
    write_reg(SE_GENI_M_IRQ_CLEAR, 0xFFFF_FFFF);
    write_reg(SE_GENI_S_IRQ_CLEAR, 0xFFFF_FFFF);

    // FIFO (not DMA) mode.
    write_reg(SE_GENI_DMA_MODE_EN, read_reg(SE_GENI_DMA_MODE_EN) & !GENI_DMA_MODE_EN);

    // 8-bit chars, 4 bytes per FIFO word, byte 0 transmitted first.
    write_reg(SE_GENI_TX_PACKING_CFG0, PACKING_CFG0_VALUE);
    write_reg(SE_GENI_TX_PACKING_CFG1, PACKING_CFG1_VALUE);
    write_reg(SE_GENI_BYTE_GRAN, 0);

    // 8 data bits, 1 stop bit, no parity, no flow control.
    write_reg(SE_UART_TX_WORD_LEN, 8);
    write_reg(SE_UART_TX_STOP_BIT_LEN, 0);
    write_reg(SE_UART_TX_TRANS_CFG, UART_CTS_MASK);
    write_reg(SE_UART_TX_PARITY_CFG, 0);

    // RX path: same packing, no parity, stale timeout as Linux uses.
    write_reg(SE_GENI_RX_PACKING_CFG0, PACKING_CFG0_VALUE);
    write_reg(SE_GENI_RX_PACKING_CFG1, PACKING_CFG1_VALUE);
    write_reg(SE_UART_RX_WORD_LEN, 8);
    write_reg(SE_UART_RX_TRANS_CFG, 0);
    write_reg(SE_UART_RX_PARITY_CFG, 0);
    write_reg(SE_UART_RX_STALE_CNT, RX_STALE_COUNT);

    // Watermarks: TX room and RX fill level. RX is polled, so the mask only
    // keeps the hardware status meaningful.
    write_reg(SE_GENI_TX_WATERMARK_REG, TX_WATERMARK);
    write_reg(SE_GENI_RX_WATERMARK_REG, RX_WATERMARK);

    // Start the secondary (receive) sequencer once; it keeps filling the RX
    // FIFO until it is aborted.
    start_read();
    READY.store(true, Ordering::Release);
}

/// Tablet default, kept for the existing SM8650 path.
pub fn init() {
    init_at(0x0089_C000);
}

/// Set when a TX poll exhausts its budget. The UART is not clocked on the
/// first fog unit, and every stuck byte used to cost an enormous spin (the
/// boot lost tens of seconds to console output). After one timeout the driver
/// stops transmitting instead of stalling each following byte.
static TX_STALLED: AtomicBool = AtomicBool::new(false);

/// Transmits a single byte by driving the main sequencer through a one-byte
/// transfer, then waits for it to drain. Polling only, no interrupts.
pub fn write_byte(byte: u8) {
    if !READY.load(Ordering::Acquire) || TX_STALLED.load(Ordering::Acquire) {
        return;
    }
    // Hand the sequencer a one-byte transfer and start it.
    write_reg(SE_UART_TX_TRANS_LEN, 1);
    write_reg(SE_GENI_M_CMD0, UART_START_TX << M_OPCODE_SHFT);

    // Wait for FIFO space (watermark status bit).
    if !poll_status(SE_GENI_M_IRQ_STATUS, M_TX_FIFO_WATERMARK_EN, true) {
        TX_STALLED.store(true, Ordering::Release);
        return;
    }

    // Push the byte. With 4x8 packing, byte 0 of the FIFO word is the LSB.
    write_reg(SE_GENI_TX_FIFON, u32::from(byte));
    write_reg(SE_GENI_M_IRQ_CLEAR, M_TX_FIFO_WATERMARK_EN);

    // Wait for the transfer to complete, then acknowledge it.
    if !poll_status(SE_GENI_M_IRQ_STATUS, M_CMD_DONE_EN, true) {
        TX_STALLED.store(true, Ordering::Release);
        return;
    }
    write_reg(SE_GENI_M_IRQ_CLEAR, M_CMD_DONE_EN);
}

pub fn write_str(message: &str) {
    write_bytes(message.as_bytes());
}

pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}

pub fn write_hex_u64(value: u64) {
    for shift in (0..64).step_by(4).rev() {
        let digit = ((value >> shift) & 0xF) as u8;
        let byte = match digit {
            0x0..=0x9 => b'0' + digit,
            _ => b'A' + digit - 0xA,
        };
        write_byte(byte);
    }
}

pub fn write_dec_u64(value: u64) {
    let mut digits = [0_u8; 20];
    let mut count = 0;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
        if rest == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        write_byte(digits[count]);
    }
}

/// Polls one received byte. Never blocks: returns `None` when the RX FIFO is
/// empty, so `SYS_READ` reports "no input" instead of hanging.
pub fn read_byte() -> Option<u8> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }

    let mut count = RX_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        // Acknowledge stale RX status before looking at the FIFO.
        let status = read_reg(SE_GENI_S_IRQ_STATUS);
        if status != 0 {
            write_reg(SE_GENI_S_IRQ_CLEAR, status);
        }

        let fifo_status = read_reg(SE_GENI_RX_FIFO_STATUS);
        let words = fifo_status & RX_FIFO_WC_MSK;
        if words == 0 {
            // The secondary sequencer stops after a transfer; restart it when
            // it is idle so a later keystroke still arrives.
            if read_reg(SE_GENI_S_CMD0) & OPCODE_MASK == 0 {
                start_read();
            }
            return None;
        }

        let word = read_reg(SE_GENI_RX_FIFON);
        RX_WORD.store(word, Ordering::Relaxed);
        count = 4;
        if fifo_status & RX_LAST != 0 {
            let valid = (fifo_status & RX_LAST_BYTE_VALID_MSK) >> 28;
            if valid != 0 {
                // The field is 3 bits but a FIFO word is four bytes; values
                // 5..7 are reserved/undefined. Clamp instead of emitting
                // bytes from beyond the word.
                count = valid.min(4);
            }
        }
        RX_COUNT.store(count, Ordering::Relaxed);
    }

    count -= 1;
    RX_COUNT.store(count, Ordering::Relaxed);
    let word = RX_WORD.load(Ordering::Relaxed);
    RX_WORD.store(word >> 8, Ordering::Relaxed);
    let byte = (word & 0xFF) as u8;
    if byte == 0x1A {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }
    Some(byte)
}

/// RX is polled from `read_byte`; no interrupt is wired for the first PoC.
pub fn handle_irq() {}

pub fn take_stop_request() -> bool {
    STOP_REQUESTED.swap(false, Ordering::Relaxed)
}

fn start_read() {
    let mut command = read_reg(SE_GENI_S_CMD0);
    command &= !OPCODE_MASK;
    command |= UART_START_READ << S_OPCODE_SHFT;
    write_reg(SE_GENI_S_CMD0, command);
}

/// Polls `SE_GENI_M_IRQ_STATUS` until `field` reads as `set`, with a bounded
/// spin so a missing/disabled UART can never wedge the kernel forever.
/// Returns `false` when the budget is exhausted; the caller then stops
/// transmitting instead of paying the same cost for every later byte.
fn poll_status(offset: usize, field: u32, set: bool) -> bool {
    let mut spins = 0_u32;
    loop {
        let value = read_reg(offset);
        if ((value & field) != 0) == set {
            return true;
        }
        spins = spins.wrapping_add(1);
        if spins == 500_000 {
            // Abort the stuck transfer so a later write can retry cleanly.
            if offset == SE_GENI_M_IRQ_STATUS {
                write_reg(SE_GENI_M_CMD_CTRL_REG, M_GENI_CMD_ABORT);
            }
            return false;
        }
        core::hint::spin_loop();
    }
}

#[inline(always)]
fn read_reg(offset: usize) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return 0;
    }
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

#[inline(always)]
fn write_reg(offset: usize, value: u32) {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}
