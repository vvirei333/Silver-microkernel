use core::ptr;

use crate::{exceptions, memory, console, scheduler, timer, platform, gicv3};

pub const TIMER_IRQ: u32 = 30;
/// QEMU virt DTB: PL011 is SPI 1, so its GIC interrupt ID is 32 + 1.
pub const UART_IRQ: u32 = 33;
const SPURIOUS_IRQ: u32 = 1023;
/// Reschedule inter-processor interrupt (SGI 0). Any CPU that makes a task
/// Ready kicks every other CPU so it re-evaluates its run queue.
pub const RESCHEDULE_IRQ: u32 = 0;

const DISTRIBUTOR_BASE: usize = memory::phys_to_virt(0x0800_0000);
const CPU_INTERFACE_BASE: usize = memory::phys_to_virt(0x0801_0000);

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ISPENDR: usize = 0x200;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xC00;
const GICD_SGIR: usize = 0xF00;

const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_BPR: usize = 0x008;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;

/// Shared distributor setup: runs once on the boot CPU.
pub fn init() {
    if platform::detect().is_tablet() {
        gicv3::init();
        return;
    }

    write32(DISTRIBUTOR_BASE + GICD_CTLR, 0);

    // The timer PPI is a banked interrupt (INTID < 32): its enable bit is
    // per-CPU, so it is enabled in `cpu_init`, not here.
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

    // BISECT: monolithic original ordering (timer enable before GICD/GICC enable)
    enable_irq(TIMER_IRQ);
    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(DISTRIBUTOR_BASE + GICD_CTLR, 1);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);
}

/// Per-CPU interrupt controller setup: the banked SGI/PPI enables (INTIDs
/// 0-31 are per-CPU in GICv2) and the private CPU interface. Runs on every
/// CPU, including APs.
pub fn cpu_init() {
    if platform::detect().is_tablet() {
        gicv3::cpu_init();
        return;
    }

    // Banked enable: SGI0 (reschedule) and this CPU's timer PPI.
    enable_irq(RESCHEDULE_IRQ);
    enable_irq(TIMER_IRQ);

    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);
}

/// P3 diagnostic: one coherent snapshot of the physical-timer PPI delivery
/// state, taken at EL1 inside a syscall (IRQs masked). READ-ONLY: no GIC
/// register with side effects is written. Marker (word 7) is stored last.
pub fn diag_timer_snapshot() {
    let isr: u64;
    let cntp_ctl: u64;
    let mpidr: u64;
    unsafe {
        core::arch::asm!("mrs {}, isr_el1", out(reg) isr, options(nomem, nostack));
        core::arch::asm!("mrs {}, cntp_ctl_el0", out(reg) cntp_ctl, options(nomem, nostack));
        core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack));
    }
    crate::smp::DIAG_SNAP.store_volatile(0, isr as u32);
    crate::smp::DIAG_SNAP.store_volatile(1, cntp_ctl as u32);
    if platform::detect().is_tablet() {
        // GICv3 with affinity routing: PPI pending/enable live in the
        // banked GICR of THIS PE (SGI frame), located via GICR_TYPER.
        let (pend, en, ctlr, pmr) = gicv3::diag_redistributor_snapshot();
        crate::smp::DIAG_SNAP.store_volatile(2, pend);
        crate::smp::DIAG_SNAP.store_volatile(3, en);
        crate::smp::DIAG_SNAP.store_volatile(4, ctlr);
        crate::smp::DIAG_SNAP.store_volatile(5, pmr);
    } else {
        // GICv2: SGIs+PPIs are banked; ISPENDR0/ISENABLER0 hold the PPI
        // state of the calling PE.
        let ppi_bit = 1_u32 << (TIMER_IRQ % 32);
        let pend = unsafe { ptr::read_volatile((DISTRIBUTOR_BASE + GICD_ISPENDR) as *const u32) };
        let en = unsafe { ptr::read_volatile((DISTRIBUTOR_BASE + GICD_ISENABLER) as *const u32) };
        let ctlr = unsafe { ptr::read_volatile((CPU_INTERFACE_BASE + GICC_CTLR) as *const u32) };
        let pmr = unsafe { ptr::read_volatile((CPU_INTERFACE_BASE + GICC_PMR) as *const u32) };
        crate::smp::DIAG_SNAP.store_volatile(2, pend & ppi_bit);
        crate::smp::DIAG_SNAP.store_volatile(3, en & ppi_bit);
        crate::smp::DIAG_SNAP.store_volatile(4, ctlr);
        crate::smp::DIAG_SNAP.store_volatile(5, pmr);
    }
    crate::smp::DIAG_SNAP.store_volatile(6, mpidr as u32);
    // Completion marker LAST: distinguishes a finished snapshot from zeros.
    crate::smp::DIAG_SNAP.store_volatile(7, 0xD14A_6001);
}

/// Signals `intid` (an SGI) to every CPU except the caller.
/// GICD_SGIR filter 0b01 = "all CPUs except the writer".
pub fn send_sgi_all_but_self(intid: u32) {
    if platform::detect().is_tablet() {
        gicv3::send_sgi_all_but_self(intid);
        return;
    }
    write32(DISTRIBUTOR_BASE + GICD_SGIR, (0b01_u32 << 24) | (intid & 0x0F));
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

    crate::smp::probe(19);
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    if irq != TIMER_IRQ && irq != RESCHEDULE_IRQ && irq != SPURIOUS_IRQ {
        // The probe and the wake must be one kernel-lock-protected step:
        // another CPU could otherwise change the binding in between.
        let _bkl = crate::smp::BklGuard::new();
        if scheduler::irq_is_bound(irq) {
            disable_irq(irq);
            end_interrupt(acknowledge_register);
            scheduler::wake_bound_irq(irq);
            return;
        }
    }

    match irq {
        RESCHEDULE_IRQ => {
            // Another CPU made a task Ready (or otherwise changed scheduler
            // state): re-evaluate our run queue. EOI first so the SGI can be
            // re-raised while we schedule.
            end_interrupt(acknowledge_register);
            scheduler::schedule();
        }
        TIMER_IRQ => {
            crate::smp::probe(20);
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
