//! Polled SDHCI core with a small board-quirk layer.
//!
//! The Pi 5 SD slot is `sdio1` in the BCM2712 tree (`brcm,bcm2712-sdhci`),
//! not a device behind RP1. The register model is standard SDHCI; only the
//! pin mux and the base-clock fallback are BCM2712-specific. This driver uses
//! PIO, one command and one 512-byte block at a time: the SFS path runs with
//! IRQs disabled and only needs a synchronous sector device.
//!
//! It does not implement tuning, SD Express, CQE, or UHS modes. It brings the
//! card up at 3.3 V, switches to 4-bit at 25 MHz, and uses block addressing for
//! SDHC/SDXC (byte addressing for older SDSC). Unsupported cards fail closed.

use core::ptr::{read_volatile, write_volatile};

use crate::console;
use crate::fdt::Fdt;
use crate::memory::phys_to_virt;
use crate::mmu;
use crate::smp::SpinLock;
use silver_microkernel::sdhci_protocol;

const BLOCK_SIZE: usize = 0x04;
const BLOCK_COUNT: usize = 0x06;
const ARGUMENT: usize = 0x08;
const TRANSFER_MODE: usize = 0x0c;
const COMMAND: usize = 0x0e;
const RESPONSE: usize = 0x10;
const BUFFER: usize = 0x20;
const PRESENT_STATE: usize = 0x24;
const HOST_CONTROL: usize = 0x28;
const POWER_CONTROL: usize = 0x29;
const CLOCK_CONTROL: usize = 0x2c;
const TIMEOUT_CONTROL: usize = 0x2e;
const SOFTWARE_RESET: usize = 0x2f;
const NORMAL_INT_STATUS: usize = 0x30;
const ERROR_INT_STATUS: usize = 0x32;
const NORMAL_INT_ENABLE: usize = 0x34;
const ERROR_INT_ENABLE: usize = 0x36;
const SIGNAL_ENABLE: usize = 0x38;
const HOST_CONTROL2: usize = 0x3e;
const CAPABILITIES: usize = 0x40;
const VERSION: usize = 0xfe;

const INT_COMMAND_COMPLETE: u16 = 1 << 0;
const INT_DATA_COMPLETE: u16 = 1 << 1;
const INT_BUFFER_WRITE_READY: u16 = 1 << 4;
const INT_BUFFER_READ_READY: u16 = 1 << 5;

const PRESENT_CMD_INHIBIT: u32 = 1 << 0;
const PRESENT_DAT_INHIBIT: u32 = 1 << 1;

const RESPONSE_NONE: u16 = 0;
const RESPONSE_136: u16 = 0x01;
const RESPONSE_48: u16 = 0x02;
const RESPONSE_48_BUSY: u16 = 0x03;
const COMMAND_CRC: u16 = 0x08;
const COMMAND_INDEX: u16 = 0x10;
const COMMAND_DATA: u16 = 0x20;

const ACMD41_ARGUMENT: u32 = 0x00ff_8000;
const ACMD6_ARGUMENT: u32 = 2;

/// Board-specific quirks on top of the standard SDHCI register model.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Generic,
    Bcm2712,
}

/// SDHCI bindings this polled core can drive. The list is deliberately
/// ordered by quirk: a specific BCM node is preferred over a generic one.
const COMPATIBLES: &[(&str, Variant)] = &[
    ("brcm,bcm2712-sdhci", Variant::Bcm2712),
    ("brcm,bcm2835-sdhci", Variant::Generic),
    ("arasan,sdhci-5.1", Variant::Generic),
    ("snps,dwcmshc-sdhci", Variant::Generic),
];

#[derive(Clone, Copy)]
struct Host {
    base: usize,
    variant: Variant,
}

#[derive(Clone, Copy)]
struct Card {
    sectors: u64,
    high_capacity: bool,
}

static mut HOST: Option<Host> = None;
static mut CARD: Option<Card> = None;
/// SFS may issue two sector operations from different tasks; the register
/// window has a single command/data state machine, so transfers serialize.
static IO_LOCK: SpinLock<()> = SpinLock::new(());

impl Host {
    fn read8(&self, offset: usize) -> u8 {
        unsafe { read_volatile((self.base + offset) as *const u8) }
    }

    fn read16(&self, offset: usize) -> u16 {
        unsafe { read_volatile((self.base + offset) as *const u16) }
    }

    fn read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write8(&self, offset: usize, value: u8) {
        unsafe { write_volatile((self.base + offset) as *mut u8, value) }
    }

    fn write16(&self, offset: usize, value: u16) {
        unsafe { write_volatile((self.base + offset) as *mut u16, value) }
    }

    fn write32(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn clear_interrupts(&self) {
        self.write16(NORMAL_INT_STATUS, 0xffff);
        self.write16(ERROR_INT_STATUS, 0xffff);
    }

    fn reset(&self) -> bool {
        self.write8(SOFTWARE_RESET, 1);
        for _ in 0..1000 {
            if self.read8(SOFTWARE_RESET) == 0 {
                return true;
            }
            delay_us(100);
        }
        false
    }

    /// Selects the SD pin mux and sets the SD clock.
    fn set_clock(&self, clock: u32) {
        self.write16(CLOCK_CONTROL, 0);
        delay_us(1000);
        let caps = self.read32(CAPABILITIES);
        let mut base_mhz = (caps >> 8) & 0xff;
        if base_mhz == 0 {
            base_mhz = if self.variant == Variant::Bcm2712 {
                200
            } else {
                50
            };
        }
        let base = base_mhz * 1_000_000;
        let divisor = sdhci_protocol::clock_divisor(base, clock);
        self.write16(CLOCK_CONTROL, divisor << 8 | 1);
        for _ in 0..1000 {
            if self.read16(CLOCK_CONTROL) & 2 != 0 {
                break;
            }
            delay_us(100);
        }
        self.write16(CLOCK_CONTROL, divisor << 8 | 5);
    }

    fn wait_present_clear(&self, mask: u32) -> bool {
        for _ in 0..10_000 {
            if self.read32(PRESENT_STATE) & mask == 0 {
                return true;
            }
            delay_us(10);
        }
        false
    }

    fn wait_command_complete(&self) -> bool {
        for _ in 0..100_000 {
            if self.read16(ERROR_INT_STATUS) != 0 {
                self.clear_interrupts();
                return false;
            }
            if self.read16(NORMAL_INT_STATUS) & INT_COMMAND_COMPLETE != 0 {
                return true;
            }
            delay_us(1);
        }
        self.clear_interrupts();
        false
    }

    fn wait_data_complete(&self) -> bool {
        for _ in 0..1_000_000 {
            if self.read16(ERROR_INT_STATUS) != 0 {
                self.clear_interrupts();
                return false;
            }
            if self.read16(NORMAL_INT_STATUS) & INT_DATA_COMPLETE != 0 {
                return true;
            }
            delay_us(1);
        }
        self.clear_interrupts();
        false
    }

    fn wait_buffer(&self, mask: u16) -> bool {
        for _ in 0..1_000_000 {
            if self.read16(ERROR_INT_STATUS) != 0 {
                self.clear_interrupts();
                return false;
            }
            if self.read16(NORMAL_INT_STATUS) & mask != 0 {
                return true;
            }
            delay_us(1);
        }
        self.clear_interrupts();
        false
    }

    fn command(&self, index: u8, argument: u32, flags: u16) -> Option<u32> {
        if !self.wait_present_clear(PRESENT_CMD_INHIBIT | PRESENT_DAT_INHIBIT) {
            return None;
        }
        self.clear_interrupts();
        self.write32(ARGUMENT, argument);
        self.write16(COMMAND, (u16::from(index) << 8) | flags);
        if !self.wait_command_complete() {
            return None;
        }
        let response = self.read32(RESPONSE);
        self.clear_interrupts();
        Some(response)
    }

    fn response(&self, index: usize) -> u32 {
        self.read32(RESPONSE + index * 4)
    }

    fn transfer(&self, address: u32, read: bool, block: &mut [u8; 512]) -> bool {
        if !self.wait_present_clear(PRESENT_CMD_INHIBIT | PRESENT_DAT_INHIBIT) {
            return false;
        }
        self.clear_interrupts();
        self.write16(BLOCK_SIZE, 512);
        self.write16(BLOCK_COUNT, 1);
        self.write32(ARGUMENT, address);
        self.write16(TRANSFER_MODE, if read { 0x02 | 0x10 } else { 0x02 });
        let command: u8 = if read { 17 } else { 24 };
        self.write16(
            COMMAND,
            (u16::from(command) << 8) | COMMAND_DATA | COMMAND_INDEX | COMMAND_CRC | RESPONSE_48,
        );
        if !self.wait_command_complete() {
            return false;
        }
        if read {
            for word in 0..128 {
                if !self.wait_buffer(INT_BUFFER_READ_READY) {
                    return false;
                }
                let value = self.read32(BUFFER);
                block[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
        } else {
            for word in 0..128 {
                if !self.wait_buffer(INT_BUFFER_WRITE_READY) {
                    return false;
                }
                let value = u32::from_le_bytes([
                    block[word * 4],
                    block[word * 4 + 1],
                    block[word * 4 + 2],
                    block[word * 4 + 3],
                ]);
                self.write32(BUFFER, value);
            }
        }
        let ok = self.wait_data_complete();
        self.clear_interrupts();
        ok
    }

    fn bring_up(&self) -> Option<Card> {
        if !self.reset() {
            return None;
        }
        self.write8(TIMEOUT_CONTROL, 0x0e);
        self.write8(HOST_CONTROL, 0);
        self.write16(HOST_CONTROL2, 0);
        // Polling still needs the status bits to be enabled: QEMU only sets
        // them when the matching interrupt-enable bit is on. Signal enable
        // stays clear, so no IRQ reaches the GIC.
        self.write16(
            NORMAL_INT_ENABLE,
            INT_COMMAND_COMPLETE
                | INT_DATA_COMPLETE
                | INT_BUFFER_WRITE_READY
                | INT_BUFFER_READ_READY,
        );
        self.write16(ERROR_INT_ENABLE, 0xffff);
        self.write16(SIGNAL_ENABLE, 0);
        self.clear_interrupts();

        // 3.3 V bus power. The board regulator is firmware-owned; this only
        // tells the host controller to drive the bus.
        self.write8(POWER_CONTROL, 0x0f);
        delay_ms(2);

        if self.variant == Variant::Bcm2712 {
            // SDIO_CFG_SD_PIN_SEL: route the controller to SD pins, not eMMC.
            let pin_select = self.read32(0x444);
            self.write32(0x444, (pin_select & !0x3) | 0x2);
        }
        self.set_clock(400_000);
        delay_ms(2);

        // CMD0: reset the card into idle state.
        let _ = self.command(0, 0, RESPONSE_NONE);

        // CMD8: voltage check and v2 detection. A timeout means an older
        // card, not necessarily a broken bus.
        let v2 = self
            .command(8, 0x1aa, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX)
            .is_some_and(|response| response & 0xfff == 0x1aa);
        let hcs = if v2 { 1 << 30 } else { 0 };

        let mut ocr = 0;
        let mut ready = false;
        for _ in 0..200 {
            let _ = self.command(55, 0, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX);
            if let Some(response) = self.command(41, ACMD41_ARGUMENT | hcs, RESPONSE_48) {
                ocr = response;
                if response & 0x8000_0000 != 0 {
                    ready = true;
                    break;
                }
            }
            delay_ms(5);
        }
        if !ready {
            return None;
        }

        // CMD2/CMD3: identify the card and read its RCA.
        self.command(2, 0, RESPONSE_136 | COMMAND_CRC | COMMAND_INDEX)?;
        let r6 = self.command(3, 0, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX)?;
        let rca = (r6 >> 16) & 0xffff;

        // CMD9: CSD, used for capacity. Keep all four response words before
        // the next command overwrites them; R2 is shifted by one byte.
        self.command(9, rca << 16, RESPONSE_136 | COMMAND_CRC | COMMAND_INDEX)?;
        let response = [
            self.response(0),
            self.response(1),
            self.response(2),
            self.response(3),
        ];

        self.command(7, rca << 16, RESPONSE_48_BUSY | COMMAND_CRC | COMMAND_INDEX)?;
        self.command(16, 512, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX)?;

        // ACMD6: four data lines. Only widen the host if the card accepted
        // the switch; otherwise a failed ACMD6 would corrupt every transfer.
        let acmd6_ok = self
            .command(55, rca << 16, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX)
            .is_some()
            && self
                .command(6, ACMD6_ARGUMENT, RESPONSE_48 | COMMAND_CRC | COMMAND_INDEX)
                .is_some();
        if acmd6_ok {
            self.write8(HOST_CONTROL, self.read8(HOST_CONTROL) | 0x2);
        }
        self.set_clock(25_000_000);

        let csd = sdhci_protocol::csd_from_response(response);
        let sectors = sdhci_protocol::capacity_sectors(csd)?;
        Some(Card {
            sectors,
            high_capacity: ocr & 0x4000_0000 != 0,
        })
    }
}

/// Finds an enabled SDHCI node and initializes its card.
pub fn init(dtb: Option<&[u8]>) -> bool {
    let Some(data) = dtb else {
        return false;
    };
    let Some(tree) = Fdt::new(data) else {
        return false;
    };
    let mut found = None;
    for (compatible, variant) in COMPATIBLES {
        if let Some(reg) = tree.first_enabled_reg(compatible) {
            found = Some((reg, *variant));
            break;
        }
    }
    let Some((reg, variant)) = found else {
        return false;
    };
    let Ok(start) = usize::try_from(reg.start) else {
        return false;
    };
    let Ok(size) = usize::try_from(reg.size()) else {
        return false;
    };
    init_at(start, size, variant)
}

/// Binds a controller at a known physical base. The DTB path sizes the
/// window from `reg`; a PCI function passes the BAR size it decoded.
pub(crate) fn init_at(start: usize, size: usize, variant: Variant) -> bool {
    // BCM2712 keeps the SD pin mux in a separate cfg window at +0x400;
    // require that too. Generic hosts only need their own reg window.
    let probe_size = if variant == Variant::Bcm2712 {
        size.max(0x600)
    } else {
        size
    };
    if !mmu::device_window_mapped(start, probe_size) {
        console::write_str("SDHCI: window not mapped\r\n");
        return false;
    }
    let host = Host {
        base: phys_to_virt(start),
        variant,
    };
    console::write_str("SDHCI: base=0x");
    write_hex(start as u64);
    console::write_str(if variant == Variant::Bcm2712 {
        " variant=BCM2712"
    } else {
        " variant=generic"
    });
    console::write_str(" version=0x");
    write_hex(u64::from(host.read16(VERSION)));
    console::write_str(" caps=0x");
    write_hex(u64::from(host.read32(CAPABILITIES)));
    console::write_str("\r\n");
    let Some(card) = host.bring_up() else {
        console::write_str("SDHCI: card init failed\r\n");
        return false;
    };
    console::write_str("SDHCI: card sectors=");
    console::write_dec_u64(card.sectors);
    console::write_str(if card.high_capacity {
        " block-addressed\r\n"
    } else {
        " byte-addressed\r\n"
    });
    unsafe {
        let host_slot = core::ptr::addr_of_mut!(HOST);
        *host_slot = Some(host);
        let card_slot = core::ptr::addr_of_mut!(CARD);
        *card_slot = Some(card);
    }
    true
}

pub fn sector_count() -> u64 {
    unsafe { core::ptr::addr_of!(CARD).read() }
        .map(|card| card.sectors)
        .unwrap_or(0)
}

pub fn read_sector(sector: u64, buffer: &mut [u8; 512]) -> bool {
    transfer(sector, true, buffer)
}

pub fn write_sector(sector: u64, buffer: &[u8; 512]) -> bool {
    let mut copy = *buffer;
    transfer(sector, false, &mut copy)
}

fn transfer(sector: u64, read: bool, buffer: &mut [u8; 512]) -> bool {
    let _guard = IO_LOCK.lock();
    let host = unsafe { core::ptr::addr_of!(HOST).read() };
    let card = unsafe { core::ptr::addr_of!(CARD).read() };
    let (Some(host), Some(card)) = (host, card) else {
        return false;
    };
    let address = if card.high_capacity {
        sector
    } else {
        sector.saturating_mul(512)
    };
    let Ok(address) = u32::try_from(address) else {
        return false;
    };
    host.transfer(address, read, buffer)
}

fn write_hex(value: u64) {
    let mut digits = [0_u8; 16];
    for (index, slot) in digits.iter_mut().enumerate() {
        let shift = (15 - index) * 4;
        let digit = ((value >> shift) & 0xf) as u8;
        *slot = match digit {
            0..=9 => b'0' + digit,
            _ => b'A' + digit - 10,
        };
    }
    let mut first = 0;
    while first + 1 < digits.len() && digits[first] == b'0' {
        first += 1;
    }
    console::write_bytes(&digits[first..]);
}

fn delay_us(microseconds: u64) {
    let frequency: u64;
    let start: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nostack));
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) start, options(nostack));
    }
    if frequency == 0 {
        for _ in 0..microseconds.saturating_mul(100) {
            core::hint::spin_loop();
        }
        return;
    }
    let ticks = microseconds.saturating_mul(frequency) / 1_000_000;
    loop {
        let now: u64;
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now, options(nostack));
        }
        if now.wrapping_sub(start) >= ticks {
            break;
        }
        core::hint::spin_loop();
    }
}

fn delay_ms(milliseconds: u64) {
    delay_us(milliseconds.saturating_mul(1000));
}
