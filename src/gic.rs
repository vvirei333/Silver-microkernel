//! Interrupt dispatch for the memory-mapped GICv2 path.
//!
//! ## Acknowledge, dispatch, EOI and DIR order (GICv2)
//!
//! 1. Read `GICC_IAR`. The returned 32-bit acknowledge register carries the
//!    INTID in bits [9:0] and the source CPU in bits [12:10].
//! 2. INTID 1023 is spurious: return without writing `GICC_EOIR`. The
//!    architecture forbids EOI for a spurious acknowledge.
//! 3. Classify the INTID: SGI (0-15), PPI (16-31), SPI (32-1019),
//!    spurious/reserved. SGI 0 is the reschedule IPI, the timer is a PPI,
//!    and the console UART is an SPI.
//! 4. For a bound or unknown interrupt, mask the *specific* INTID before EOI:
//!    `GICD_ICENABLER0` for SGIs/PPIs (banked per CPU) and the matching
//!    `GICD_ICENABLER<n>` word for SPIs. There is no universal mask write.
//! 5. Write the full acknowledge register (not just the INTID) to
//!    `GICC_EOIR`.
//! 6. `GICC_DIR` is unused here because `GICC_CTLR.EOImode` is 0. With split
//!    priority drop/deactivation disabled, the EOIR write both drops the
//!    active priority and deactivates the interrupt in one step.
//!
//! A bound driver normally runs with its INTID disabled and re-enables it
//! from `wait_irq`, so the kernel wakes the task after masking.

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::irq_policy::{self, MaskScope, UnmaskableAction};
use crate::{exceptions, memory, console, scheduler, timer, platform, gicv3};

/// Non-secure physical timer PPI. QEMU virt and SM8650 use PPI 14 (INTID 30);
/// SM6225/khaje wires the ARMv8 timer as PPI 2 (INTID 18).
#[cfg(feature = "fog")]
pub const TIMER_IRQ: u32 = 18;
#[cfg(not(feature = "fog"))]
pub const TIMER_IRQ: u32 = 30;
/// QEMU virt DTB: PL011 is SPI 1, so its GIC interrupt ID is 32 + 1.
/// SM6225 SE4 debug UART is SPI 331, so INTID 32 + 331 = 363.
#[cfg(feature = "fog")]
pub const UART_IRQ: u32 = 363;
#[cfg(not(feature = "fog"))]
pub const UART_IRQ: u32 = 33;
const SPURIOUS_IRQ: u32 = 1023;
/// Reschedule inter-processor interrupt (SGI 0). Any CPU that makes a task
/// Ready kicks every other CPU so it re-evaluates its run queue.
pub const RESCHEDULE_IRQ: u32 = 0;
/// Stop-the-world acknowledgement SGI. A CPU receiving this parks at EL1
/// until the quiescing CPU releases the world.
pub const QUIESCE_IRQ: u32 = 1;

/// Consecutive unknown INTIDs that could not be masked in a proven scope.
/// Reset by a successful mask. See `irq_policy::UNMASKABLE_HALT_LIMIT`.
static UNMASKABLE_STREAK: AtomicU32 = AtomicU32::new(0);
/// Diagnostic counter: log only the first few masked unknown INTIDs so a
/// broken device cannot flood the console from the IRQ path.
static UNKNOWN_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const UNKNOWN_LOG_LIMIT: u32 = 4;

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
/// INTID of the non-secure physical timer for the active controller.
pub fn timer_irq() -> u32 {
    if platform::detect().uses_gicv3() {
        gicv3::timer_irq()
    } else {
        TIMER_IRQ
    }
}

/// INTID of the console UART for the active controller.
pub fn uart_irq() -> u32 {
    if platform::detect().uses_gicv3() {
        gicv3::uart_irq()
    } else {
        UART_IRQ
    }
}

/// Whether the active interrupt controller can actually deliver the timer
/// PPI. GICv2 is memory-mapped and always usable here; GICv3 depends on
/// finding this CPU's redistributor by affinity.
pub fn timer_available() -> bool {
    if platform::detect().uses_gicv3() {
        gicv3::timer_available()
    } else {
        platform::detect() != platform::Kind::Unknown
    }
}

pub fn init() {
    if platform::detect().uses_gicv3() {
        gicv3::init();
        return;
    }
    if platform::detect() == platform::Kind::Unknown {
        // No proven interrupt controller: leave interrupts masked. The boot
        // path still reaches its diagnostic halt instead of writing to an
        // unverified distributor address.
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

    // Boot CPU's banked SGIs: reschedule and quiesce. APs get the same set
    // from `cpu_init`.
    enable_irq(RESCHEDULE_IRQ);
    enable_irq(QUIESCE_IRQ);
    // BISECT: monolithic original ordering (timer enable before GICD/GICC enable)
    enable_irq(TIMER_IRQ);
    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(DISTRIBUTOR_BASE + GICD_CTLR, 1);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);
}

/// Per-CPU interrupt controller setup: the banked SGI/PPI enables (INTIDs
/// 0-31 are per-CPU in GICv2) and the private CPU interface. Runs on every
/// CPU, including APs. Returns false when the CPU cannot receive SGIs and
/// therefore must not be published as online.
pub fn cpu_init() -> bool {
    if platform::detect().uses_gicv3() {
        return gicv3::cpu_init();
    }

    // Banked enable: SGI0 (reschedule), SGI1 (quiesce) and this CPU's timer PPI.
    enable_irq(RESCHEDULE_IRQ);
    enable_irq(QUIESCE_IRQ);
    enable_irq(TIMER_IRQ);

    write32(CPU_INTERFACE_BASE + GICC_PMR, 0xFF);
    write32(CPU_INTERFACE_BASE + GICC_BPR, 0);
    write32(CPU_INTERFACE_BASE + GICC_CTLR, 1);

    // Banked SGI enables must read back on this PE before it is published:
    // an AP without SGI0/SGI1 cannot take part in rescheduling or quiescence.
    let expected = (1_u32 << RESCHEDULE_IRQ) | (1_u32 << QUIESCE_IRQ);
    read32(DISTRIBUTOR_BASE + GICD_ISENABLER) & expected == expected
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
    if platform::detect().uses_gicv3() {
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
    if platform::detect().uses_gicv3() {
        gicv3::send_sgi_all_but_self(intid);
        return;
    }
    write32(DISTRIBUTOR_BASE + GICD_SGIR, (0b01_u32 << 24) | (intid & 0x0F));
}

pub fn enable_irq(irq: u32) {
    if platform::detect().uses_gicv3() {
        gicv3::enable_irq(irq);
        return;
    }
    let offset = GICD_ISENABLER + (irq as usize / 32) * 4;
    write32(DISTRIBUTOR_BASE + offset, 1_u32 << (irq % 32));
}

pub fn disable_irq(irq: u32) {
    if platform::detect().uses_gicv3() {
        gicv3::disable_irq(irq);
        return;
    }
    let offset = GICD_ICENABLER + (irq as usize / 32) * 4;
    write32(DISTRIBUTOR_BASE + offset, 1_u32 << (irq % 32));
}

/// Masks one INTID in the register scope that owns it and confirms the
/// enable bit is clear. Returns false when no proven scope exists, in which
/// case the caller must use the bounded unmaskable fallback.
pub fn mask_irq(intid: u32) -> bool {
    if platform::detect().uses_gicv3() {
        return gicv3::mask_irq(intid);
    }
    if irq_policy::mask_scope(intid) == MaskScope::Unavailable {
        return false;
    }
    let bit = 1_u32 << (intid % 32);
    let offset = (intid as usize / 32) * 4;
    // SGI/PPI enables are banked per CPU in word 0; SPIs live in the
    // distributor's shared words. Both use ISENABLER/ICENABLER at the same
    // offsets, but the write must go to the matching scope.
    write32(DISTRIBUTOR_BASE + GICD_ICENABLER + offset, bit);
    read32(DISTRIBUTOR_BASE + GICD_ISENABLER + offset) & bit == 0
}

/// Diagnostic + bounded fallback for an unknown, unbound INTID.
///
/// Returns true when the kernel may continue. `masked` says whether
/// [`mask_irq`] proved the INTID disabled; when it did, the storm is stopped
/// and the streak resets. When it did not, the caller is about to take the
/// same interrupt again, so only a finite number of attempts is permitted
/// before the exception path halts the machine.
pub fn note_unknown_irq(intid: u32, masked: bool) -> bool {
    // The tablet diagnostic path logs a fixed string without the numeric ID.
    #[cfg(feature = "tablet")]
    let _ = intid;
    if masked {
        UNMASKABLE_STREAK.store(0, Ordering::Relaxed);
        let seen = UNKNOWN_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if seen < UNKNOWN_LOG_LIMIT {
            #[cfg(feature = "tablet")]
            crate::ram_log::log("IRQ: masked unbound INTID");
            #[cfg(not(feature = "tablet"))]
            {
                console::write_str("IRQ: masked unbound INTID ");
                console::write_hex_u64(u64::from(intid));
                console::write_str("\r\n");
            }
        }
        return true;
    }

    let streak = UNMASKABLE_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
    let halt = irq_policy::unmaskable_streak_action(streak) == UnmaskableAction::Halt;
    // Log the first failure and the final one; the middle attempts would
    // otherwise produce a short but pointless burst.
    if streak == 1 || halt {
        #[cfg(feature = "tablet")]
        crate::ram_log::log("IRQ: unbound INTID cannot be masked");
        #[cfg(not(feature = "tablet"))]
        {
            console::write_str("IRQ: unbound INTID cannot be masked ");
            console::write_hex_u64(u64::from(intid));
            console::write_str("\r\n");
        }
    }
    !halt
}

pub fn acknowledge() -> u32 {
    if platform::detect().uses_gicv3() {
        return gicv3::acknowledge();
    }
    read32(CPU_INTERFACE_BASE + GICC_IAR)
}

pub fn end_interrupt(acknowledge_register: u32) {
    if platform::detect().uses_gicv3() {
        gicv3::end_interrupt(acknowledge_register);
        return;
    }
    write32(CPU_INTERFACE_BASE + GICC_EOIR, acknowledge_register);
}

#[no_mangle]
pub extern "C" fn silver_handle_irq() {
    if platform::detect().uses_gicv3() {
        return gicv3::handle_irq();
    }

    crate::smp::probe(19);
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    if irq != TIMER_IRQ
        && irq != RESCHEDULE_IRQ
        && irq != QUIESCE_IRQ
        && irq != SPURIOUS_IRQ
    {
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
        QUIESCE_IRQ => {
            end_interrupt(acknowledge_register);
            crate::smp::quiesce_ack_and_wait();
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
            // No driver owns this INTID. Mask the specific line in its own
            // scope first, then EOI so the active state is released. If the
            // mask cannot be proven, the bounded fallback below halts
            // instead of accepting an endless storm.
            let masked = mask_irq(irq);
            end_interrupt(acknowledge_register);
            if !note_unknown_irq(irq, masked) {
                exceptions::silver_handle_exception(8, u64::from(irq), 0, 0);
            }
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
