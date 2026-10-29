//! Qualcomm GENI SE (Serial Engine) UART driver — polled, TX-first.
#![allow(dead_code)] // Tablet driver, not yet wired into the boot path (QEMU uses PL011).
//!
//! The Legion Y700 (SM8650 "pineapple") debug console is a GENI UART at the
//! physical address `0x0089_C000` (DTB node `qupv3_se15_2uart`, `stdout-path`),
//! baud 115200 8n1. Unlike the PL011 on QEMU `virt`, the GENI UART is driven by
//! a small state machine: the main sequencer is handed a transfer length and a
//! `START_TX` opcode, the bytes are pushed into a word-based FIFO, and the
//! sequencer drains the FIFO to the line on its own.
//!
//! The register offsets and bit fields below mirror the Linux
//! `qcom_geni_serial.c` / `geni-se.h` (the offsets are hardware, not API).
//! This driver is deliberately minimal: it configures the UART protocol, the
//! 8-bit/4-word FIFO packing and the watermark, then transmits one byte at a
//! time by polling the FIFO-watermark and command-done status bits. It relies
//! on the bootloader (ABL) having already brought up the SE clock at the right
//! rate for 115200 8n1 — re-programming the clock tree is out of scope for the
//! first boot console.

use core::sync::atomic::{AtomicBool, Ordering};

/// The debug UART sits below 0x4000_0000, inside the first MMIO gigabyte that
/// the boot stub maps device-nGnRnE on both TTBRs, so the ordinary high-half
/// identity alias (`KERNEL_VIRT_BASE + PA`) already reaches it.
const BASE: usize = crate::memory::phys_to_virt(0x0089_C000);

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

// --- Bit fields (from Linux `geni-se.h` / `qcom_geni_serial.c`) ------------
/// `SE_GENI_STATUS`: main sequencer has a command in flight.
const M_GENI_CMD_ACTIVE: u32 = 1 << 0;
/// Opcode lives in the top 5 bits of `SE_GENI_M_CMD0`.
const M_OPCODE_SHFT: u32 = 27;
const UART_START_TX: u32 = 0x1;
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
const TX_PACKING_CFG0_VALUE: u32 = 0x0004_380E;
const TX_PACKING_CFG1_VALUE: u32 = 0x000C_5E0E;
/// `DEF_TX_WM`: fire the watermark IRQ when the FIFO has this many words left.
const TX_WATERMARK: u32 = 2;
/// Words per FIFO entry for 4x8 packing (unused for TX, documented for RX).
const _FIFO_DEPTH_WORDS: u32 = 16;

/// Stub state for the Ctrl-Z "stop console" job-control hook. RX is not wired
/// on the first boot (this driver is TX-only), so nothing ever sets this.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Configures the UART protocol and FIFO packing. The SE clock is assumed to be
/// already running at the 115200 sampling rate (set up by the bootloader).
pub fn init() {
    // Clear every GENI interrupt so no stale status bit can fake a completion.
    write_reg(SE_GENI_M_IRQ_CLEAR, 0xFFFF_FFFF);
    write_reg(SE_GENI_S_IRQ_CLEAR, 0xFFFF_FFFF);

    // FIFO (not DMA) mode.
    write_reg(SE_GENI_DMA_MODE_EN, read_reg(SE_GENI_DMA_MODE_EN) & !GENI_DMA_MODE_EN);

    // 8-bit chars, 4 bytes per FIFO word, byte 0 transmitted first.
    write_reg(SE_GENI_TX_PACKING_CFG0, TX_PACKING_CFG0_VALUE);
    write_reg(SE_GENI_TX_PACKING_CFG1, TX_PACKING_CFG1_VALUE);
    write_reg(SE_GENI_BYTE_GRAN, 0);

    // 8 data bits, 1 stop bit, no parity, no flow control.
    write_reg(SE_UART_TX_WORD_LEN, 8);
    write_reg(SE_UART_TX_STOP_BIT_LEN, 0);
    write_reg(SE_UART_TX_TRANS_CFG, UART_CTS_MASK);
    write_reg(SE_UART_TX_PARITY_CFG, 0);

    // Watermark: interrupt when the TX FIFO holds fewer than 2 words.
    write_reg(SE_GENI_TX_WATERMARK_REG, TX_WATERMARK);
}

/// Transmits a single byte by driving the main sequencer through a one-byte
/// transfer, then waits for it to drain. Polling only, no interrupts.
pub fn write_byte(byte: u8) {
    // Hand the sequencer a one-byte transfer and start it.
    write_reg(SE_UART_TX_TRANS_LEN, 1);
    write_reg(SE_GENI_M_CMD0, UART_START_TX << M_OPCODE_SHFT);

    // Wait for FIFO space (watermark status bit).
    poll_status(SE_GENI_M_IRQ_STATUS, M_TX_FIFO_WATERMARK_EN, true);

    // Push the byte. With 4x8 packing, byte 0 of the FIFO word is the LSB.
    write_reg(SE_GENI_TX_FIFON, u32::from(byte));
    write_reg(SE_GENI_M_IRQ_CLEAR, M_TX_FIFO_WATERMARK_EN);

    // Wait for the transfer to complete, then acknowledge it.
    poll_status(SE_GENI_M_IRQ_STATUS, M_CMD_DONE_EN, true);
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

/// RX is not wired on the first boot: the S sequencer (receive) is left alone.
/// Returns `None` so the `read` syscall reports "no input" instead of hanging.
pub fn read_byte() -> Option<u8> {
    None
}

/// No-op until the GENI UART interrupt (GIC SPI 462) is wired into `gic.rs`.
pub fn handle_irq() {}

pub fn take_stop_request() -> bool {
    STOP_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Polls `SE_GENI_M_IRQ_STATUS` until `field` reads as `set`, with a bounded
/// spin so a missing/disabled UART can never wedge the kernel forever.
fn poll_status(offset: usize, field: u32, set: bool) {
    let mut spins = 0_u32;
    loop {
        let value = read_reg(offset);
        if ((value & field) != 0) == set {
            return;
        }
        spins = spins.wrapping_add(1);
        if spins == 50_000_000 {
            // Abort the stuck transfer so a later write can retry cleanly.
            if offset == SE_GENI_M_IRQ_STATUS {
                write_reg(SE_GENI_M_CMD_CTRL_REG, M_GENI_CMD_ABORT);
            }
            return;
        }
        core::hint::spin_loop();
    }
}

#[inline(always)]
fn read_reg(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((BASE + offset) as *const u32) }
}

#[inline(always)]
fn write_reg(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((BASE + offset) as *mut u32, value) }
}
