use core::ptr;

use crate::{exceptions, memory, scheduler, timer};

pub const TIMER_IRQ: u32 = 30;
const SPURIOUS_IRQ: u32 = 1023;

const DISTRIBUTOR_BASE: usize = memory::phys_to_virt(0x0800_0000);
const CPU_INTERFACE_BASE: usize = memory::phys_to_virt(0x0801_0000);

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ICFGR: usize = 0xC00;

const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_BPR: usize = 0x008;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;

pub fn init() {
    write32(DISTRIBUTOR_BASE + GICD_CTLR, 0);

    write8(
        DISTRIBUTOR_BASE + GICD_IPRIORITYR + TIMER_IRQ as usize,
        0xA0,
    );

    let config_offset = GICD_ICFGR + (TIMER_IRQ as usize / 16) * 4;
    let config_shift = (TIMER_IRQ as usize % 16) * 2;
    write32(
        DISTRIBUTOR_BASE + config_offset,
        read32(DISTRIBUTOR_BASE + config_offset) & !(0b11_u32 << config_shift),
    );

    let enable_offset = GICD_ISENABLER + (TIMER_IRQ as usize / 32) * 4;
    write32(
        DISTRIBUTOR_BASE + enable_offset,
        1_u32 << (TIMER_IRQ as usize % 32),
    );

    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(DISTRIBUTOR_BASE + GICD_CTLR, 1);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);
}

pub fn acknowledge() -> u32 {
    read32(CPU_INTERFACE_BASE + GICC_IAR)
}

pub fn end_interrupt(acknowledge_register: u32) {
    write32(CPU_INTERFACE_BASE + GICC_EOIR, acknowledge_register);
}

#[no_mangle]
pub extern "C" fn silver_handle_irq() {
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    match irq {
        TIMER_IRQ => {
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            scheduler::schedule();
        }
        SPURIOUS_IRQ => {}
        _ => exceptions::silver_handle_exception(8, u64::from(irq), 0, 0),
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
