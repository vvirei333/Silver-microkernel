//! GICv3 interrupt controller for the SM8650 tablet (Lenovo Y700).
//!
//! Addresses from the device's live FDT (`tablet/tablet.dts`):
//! GICD @ 0x1710_0000 (64 KiB), GICR @ 0x1718_0000 with a 0x4_0000 stride per
//! core. Unlike GICv2 there is no memory-mapped CPU interface: the kernel uses
//! the `ICC_*` system registers directly. The kernel runs single-core at EL1,
//! so only the first redistributor (core 0) is touched.
//!
//! Timer: the non-secure physical timer PPI is DTB interrupt 14 → INTID 30.
//! UART: GENI debug UART SPI 462 → INTID 494. The interrupt is *configured*
//! (group, priority, level trigger) but not enabled: `geni_uart` is TX-only
//! polling today, so an enabled line would just raise unhandled IRQs.

use core::ptr;

use crate::{exceptions, memory, scheduler, timer};

/// Non-secure physical timer PPI (DTB 14) → GIC INTID 16 + 14.
pub const TIMER_IRQ: u32 = 30;
/// GENI SE15 debug UART SPI 462 → GIC INTID 32 + 462.
pub const UART_IRQ: u32 = 494;
const SPURIOUS_IRQ: u32 = 1023;

const GICD_BASE: usize = memory::phys_to_virt(0x1710_0000);
const GICR_BASE: usize = memory::phys_to_virt(0x1718_0000);
/// The SGI/PPI register frame is the second 64 KiB page of the redistributor
/// (the first 64 KiB is the RD control frame holding WAKER). The kernel is
/// single-core, so only the core-0 redistributor (stride 0x40000, core 0) is used.
const GICR_SGI_OFFSET: usize = 0x1_0000;

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ICFGR: usize = 0xC00;
const GICD_IGROUPR: usize = 0x080;
const GICD_IGRPMODR: usize = 0xD00;

const GICR_WAKER: usize = 0x014;
const GICR_IGROUPR0: usize = 0x080;
const GICR_ISENABLER0: usize = 0x100;
const GICR_IPRIORITYR: usize = 0x400;
const GICR_IGRPMODR0: usize = 0xD00;

const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// GICD_CTLR: Enable Group 1 (Non-secure). bit 0 (Group 0) stays off; this
/// kernel has no secure-world interrupts.
const GICD_CTLR_ENABLE_GRP1NS: u32 = 1 << 1;
const GICD_CTLR_RWP: u32 = 1 << 31;

pub fn init() {
    unsafe {
        // System-register interface for the CPU side (ICC_*_EL1), not the
        // legacy MMIO one. `isb` right after, as the spec requires.
        let sre: u64;
        core::arch::asm!("mrs {sre}, icc_sre_el1", sre = out(reg) sre, options(nostack));
        if sre & 1 == 0 {
            core::arch::asm!(
                "msr icc_sre_el1, {sre}",
                "isb",
                sre = in(reg) sre | 1,
                options(nostack),
            );
        }

        wake_redistributor();
        configure_ppi();
        configure_uart_spi();
        start_distributor();

        // CPU interface: full priority mask, EOImode 0 (one write completes the
        // interrupt), Group 1 NS signalling enabled.
        core::arch::asm!(
            "msr icc_ctlr_el1, {ctlr}",
            "msr icc_pmr_el1, {pmr}",
            "msr icc_igrpen1_el1, {grp1}",
            "isb",
            ctlr = in(reg) 0_u64,
            pmr = in(reg) 0xFF_u64,
            grp1 = in(reg) 1_u64,
            options(nomem, nostack),
        );
    }
}

/// Clears GICR_WAKER.ProcessorSleep and waits for the redistributor to come up.
/// A sleeping redistributor silently drops every interrupt, including the timer.
fn wake_redistributor() {
    let rd = GICR_BASE;
    let waker = read32(rd + GICR_WAKER);
    write32(rd + GICR_WAKER, waker & !WAKER_PROCESSOR_SLEEP);
    while read32(rd + GICR_WAKER) & WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
}

/// SGI/PPI frame of core 0 (single-core kernel): group 1 non-secure, low
/// priority, level trigger for the timer, then enable INTID 30.
fn configure_ppi() {
    let sgi = GICR_BASE + GICR_SGI_OFFSET;
    let timer_bit = 1_u32 << (TIMER_IRQ % 32);

    write32(sgi + GICR_IGROUPR0, read32(sgi + GICR_IGROUPR0) | timer_bit);
    write32(sgi + GICR_IGRPMODR0, read32(sgi + GICR_IGRPMODR0) & !timer_bit);
    // The NS physical timer asserts its line level-style while
    // CNTP_CTL.IStatus is set, which is what the reset ICFGR0 value
    // describes — no config change needed for a PPI.
    write8(sgi + GICR_IPRIORITYR + TIMER_IRQ as usize, 0xA0);
    write32(sgi + GICR_ISENABLER0, timer_bit);
}

/// Routes and classifies the UART SPI in GICD but does not enable it (see the
/// module doc): geni_uart is polling TX-only, RX IRQ support comes later.
fn configure_uart_spi() {
    let irq = UART_IRQ;
    let word = 1_u32 << (irq % 32);
    let offset = (irq as usize / 32) * 4;

    write32(GICD_BASE + GICD_IGROUPR + offset, read32(GICD_BASE + GICD_IGROUPR + offset) | word);
    write32(GICD_BASE + GICD_IGRPMODR + offset, read32(GICD_BASE + GICD_IGRPMODR + offset) & !word);
    // Level-triggered: clear the two config bits for SPI 462.
    let config_offset = GICD_ICFGR + (irq as usize / 16) * 4;
    let config_shift = (irq as usize % 16) * 2;
    write32(
        GICD_BASE + config_offset,
        read32(GICD_BASE + config_offset) & !(0b11_u32 << config_shift),
    );
    let priority_offset = GICD_IPRIORITYR + (irq as usize / 4) * 4;
    let priority_shift = (irq as usize % 4) * 8;
    write32(
        GICD_BASE + priority_offset,
        (read32(GICD_BASE + priority_offset) & !(0xFF_u32 << priority_shift))
            | (0x80_u32 << priority_shift),
    );
    // Not enabled: the GICD_ICENABLER reset state already has this bit clear.
}

fn start_distributor() {
    write32(GICD_BASE + GICD_CTLR, GICD_CTLR_ENABLE_GRP1NS);
    while read32(GICD_BASE + GICD_CTLR) & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

pub fn enable_irq(irq: u32) {
    if irq < 32 {
        // SGI/PPI frame: group routing was set at init, only the enable bit
        // changes per interrupt.
        let offset = GICR_BASE + GICR_SGI_OFFSET + GICD_ISENABLER;
        write32(offset, 1_u32 << (irq % 32));
        return;
    }
    let offset = GICD_ISENABLER + (irq as usize / 32) * 4;
    write32(GICD_BASE + offset, 1_u32 << (irq % 32));
    // A dynamically enabled SPI also needs Group 1 routing; drivers that bind
    // IRQs at runtime (scheduler::bind_irq) call this after the device is set up.
    let group_offset = GICD_IGROUPR + (irq as usize / 32) * 4;
    write32(
        GICD_BASE + group_offset,
        read32(GICD_BASE + group_offset) | 1_u32 << (irq % 32),
    );
}

pub fn disable_irq(irq: u32) {
    let base = if irq < 32 {
        GICR_BASE + GICR_SGI_OFFSET + GICD_ICENABLER
    } else {
        GICD_BASE + GICD_ICENABLER
    };
    let offset = base + (irq as usize / 32) * 4;
    write32(offset, 1_u32 << (irq % 32));
}

/// Group 1 NS acknowledge: ICC_IAR1_EL1 returns the INTID in bits [23:0].
pub fn acknowledge() -> u32 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {value}, icc_iar1_el1", value = out(reg) value, options(nomem, nostack));
    }
    value as u32 & 0x3FF
}

pub fn end_interrupt(acknowledge_register: u32) {
    unsafe {
        core::arch::asm!(
            "msr icc_eoir1_el1, {value}",
            value = in(reg) u64::from(acknowledge_register),
            options(nomem, nostack),
        );
    }
}

/// Top-level IRQ path from `exception_vectors` on the tablet. Mirrors the GICv2
/// dispatch in `gic.rs`: the timer reschedules, everything else bound to a task
/// wakes it, unknown INTIDs panic through the exception handler.
pub fn handle_irq() {
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    match irq {
        TIMER_IRQ => {
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            scheduler::schedule();
        }
        SPURIOUS_IRQ => {}
        _ => {
            if scheduler::irq_is_bound(irq) {
                disable_irq(irq);
                end_interrupt(acknowledge_register);
                scheduler::wake_bound_irq(irq);
                return;
            }
            end_interrupt(acknowledge_register);
            exceptions::silver_handle_exception(8, u64::from(irq), 0, 0);
        }
    }
}

fn read32(address: usize) -> u32 {
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    unsafe { ptr::write_volatile(address as *mut u32, value) }
}

fn write8(address: usize, value: u8) {
    unsafe { ptr::write_volatile(address as *mut u8, value) }
}
