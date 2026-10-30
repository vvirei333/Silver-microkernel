use core::ptr;

use crate::{exceptions, memory, console, scheduler, timer, platform, gicv3};

pub const TIMER_IRQ: u32 = 30;
/// QEMU virt DTB: PL011 is SPI 1, so its GIC interrupt ID is 32 + 1.
pub const UART_IRQ: u32 = 33;
const SPURIOUS_IRQ: u32 = 1023;

const DISTRIBUTOR_BASE: usize = memory::phys_to_virt(0x0800_0000);
const CPU_INTERFACE_BASE: usize = memory::phys_to_virt(0x0801_0000);

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xC00;

const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_BPR: usize = 0x008;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;

pub fn init() {
    if platform::detect().is_tablet() {
        gicv3::init();
        return;
    }

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
    enable_irq(TIMER_IRQ);

    // UART RX is level-triggered and initially routed to CPU interface 0.
    // Give input precedence over the periodic scheduler tick.
    let priority_offset = GICD_IPRIORITYR + (UART_IRQ as usize / 4) * 4;
    let priority_shift = (UART_IRQ as usize % 4) * 8;
    write32(
        DISTRIBUTOR_BASE + priority_offset,
        (read32(DISTRIBUTOR_BASE + priority_offset) & !(0xFF_u32 << priority_shift))
            | (0x80_u32 << priority_shift),
    );
    let target_offset = GICD_ITARGETSR + (UART_IRQ as usize / 4) * 4;
    write32(DISTRIBUTOR_BASE + target_offset, 0x0101_0101);
    let config_offset = GICD_ICFGR + (UART_IRQ as usize / 16) * 4;
    let config_shift = (UART_IRQ as usize % 16) * 2;
    write32(
        DISTRIBUTOR_BASE + config_offset,
        read32(DISTRIBUTOR_BASE + config_offset) & !(0b11_u32 << config_shift),
    );
    enable_irq(UART_IRQ);

    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(DISTRIBUTOR_BASE + GICD_CTLR, 1);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);
}

pub fn enable_irq(irq: u32) {
    if platform::detect().is_tablet() {
        gicv3::enable_irq(irq);
        return;
    }
    let offset = GICD_ISENABLER + (irq as usize / 32) * 4;
    write32(DISTRIBUTOR_BASE + offset, 1_u32 << (irq % 32));
}

pub fn disable_irq(irq: u32) {
    if platform::detect().is_tablet() {
        gicv3::disable_irq(irq);
        return;
    }
    let offset = GICD_ICENABLER + (irq as usize / 32) * 4;
    write32(DISTRIBUTOR_BASE + offset, 1_u32 << (irq % 32));
}

pub fn acknowledge() -> u32 {
    if platform::detect().is_tablet() {
        return gicv3::acknowledge();
    }
    read32(CPU_INTERFACE_BASE + GICC_IAR)
}

pub fn end_interrupt(acknowledge_register: u32) {
    if platform::detect().is_tablet() {
        gicv3::end_interrupt(acknowledge_register);
        return;
    }
    write32(CPU_INTERFACE_BASE + GICC_EOIR, acknowledge_register);
}

#[no_mangle]
pub extern "C" fn silver_handle_irq() {
    if platform::detect().is_tablet() {
        return gicv3::handle_irq();
    }

    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    if irq != TIMER_IRQ && irq != SPURIOUS_IRQ && scheduler::irq_is_bound(irq) {
        disable_irq(irq);
        end_interrupt(acknowledge_register);
        scheduler::wake_bound_irq(irq);
        return;
    }

    match irq {
        TIMER_IRQ => {
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            scheduler::schedule();
        }
        UART_IRQ => {
            console::handle_irq();
            let stopped = console::take_stop_request() && scheduler::stop_console_owner();
            end_interrupt(acknowledge_register);
            // Ordinary input only fills the RX queue. Ctrl-Z changes scheduler
            // state and must switch back to the shell immediately.
            if stopped {
                scheduler::schedule();
            }
        }
        SPURIOUS_IRQ => {}
        _ => {
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
