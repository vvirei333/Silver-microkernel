//! GICv3 interrupt controller for the SM8650 tablet and SM6225 Redmi 10C.
//!
//! The two SoCs share the architecture but not the addresses:
//!
//! | | SM8650 (Y700) | SM6225 (khaje) |
//! |---|---|---|
//! | GICD | 0x1710_0000 | 0x0F20_0000 |
//! | GICR | 0x1718_0000 | 0x0F30_0000 |
//! | GICR stride | 0x4_0000 | 0x2_0000 |
//! | timer PPI | 14 → INTID 30 | 2 → INTID 18 |
//! | UART SPI | 462 → INTID 494 | 331 → INTID 363 |
//!
//! Unlike GICv2 there is no memory-mapped CPU interface: the kernel uses the
//! `ICC_*` system registers directly. The kernel runs single-core at EL1, so
//! only the boot CPU's redistributor is touched. The UART interrupt is
//! *configured* but not enabled: the GENI driver polls RX/TX.
//!
//! ## Acknowledge, dispatch, EOI and DIR order (GICv3)
//!
//! 1. Read `ICC_IAR1_EL1`. The acknowledge register carries the INTID in bits
//!    [23:0] (the mask below keeps bits [9:0], which covers every INTID this
//!    kernel handles).
//! 2. INTID 1023 is spurious: return without writing `ICC_EOIR1_EL1`.
//! 3. Classify the INTID: SGI (0-15), PPI (16-31), SPI (32-1019),
//!    spurious/reserved. SGI/PPI state is banked in *this PE's* GICR frame;
//!    SPIs are routed by the distributor.
//! 4. For a bound or unknown interrupt, mask the specific INTID before EOI:
//!    `GICR_ICENABLER0` on the affinity-matched redistributor for SGIs/PPIs,
//!    and the matching `GICD_ICENABLER<n>` word for SPIs. Never write a
//!    redistributor register until `GICR_TYPER` has proven the frame belongs
//!    to this CPU.
//! 5. Write the acknowledge register to `ICC_EOIR1_EL1`.
//! 6. `ICC_DIR_EL1` is unused because `ICC_CTLR_EL1.EOImode` is 0 (set in
//!    `cpu_init_inner`). With split priority drop/deactivation disabled, the
//!    EOIR1 write both drops the active priority and deactivates the
//!    interrupt in one step.
//!
//! Readback during init proves register state only. It does not prove that a
//! device asserts its line, that the distributor routes it, or that the
//! exception vector is reached; only a real interrupt (the timer tick on
//! hardware) shows that.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::gicv3_policy::{
    bounded_wait, find_redistributor_index, mpidr_affinity, required_bits_set,
};
use crate::irq_policy::{self, MaskScope};
use crate::{exceptions, scheduler, timer};

/// Reschedule SGI, mirrors `gic::RESCHEDULE_IRQ` (the tablet is single-core
/// today, but the mechanism must stay symmetric).
pub const RESCHEDULE_IRQ: u32 = 0;
const SPURIOUS_IRQ: u32 = 1023;

/// The SGI/PPI register frame is the second 64 KiB page of the redistributor
/// (the first 64 KiB is the RD control frame holding WAKER). The kernel is
/// single-core, so only the core-0 redistributor (stride 0x40000, core 0) is used.
const GICR_SGI_OFFSET: usize = 0x1_0000;

const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IIDR: usize = 0x008;
const GICD_STATUSR: usize = 0x010;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ICFGR: usize = 0xC00;
const GICD_IGROUPR: usize = 0x080;
const GICD_IGRPMODR: usize = 0xD00;

const GICR_WAKER: usize = 0x014;
/// SGI/PPI frame: ISPENDR0 (+0x200) / ISENABLER0 (+0x100) hold the banked
/// pending/enable state of SGIs and PPIs of the owning PE.
const GICR_ISPENDR0: usize = 0x200;
/// Redistributor type register: the packed affinity (Aff3..Aff0) is in bits
/// [63:32]; bits [31:0] hold Processor_Number, Last and DirectLPI flags.
/// Frames are matched to CPUs through this register, never through the frame
/// index — the frame order does not have to follow the MPIDR values.
const GICR_TYPER: usize = 0x008;
const GICR_MAX_PE: usize = 8;

// Runtime configuration. The live FDT is authoritative; these defaults are
// only used when no tree described a GIC (or for the existing tablet path).
static GICD_BASE_PA: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static GICR_BASE_PA: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static GICR_STRIDE_RT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0x2_0000);
static GICR_MAX_PE_RT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(GICR_MAX_PE);
static TIMER_IRQ_RT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static UART_IRQ_RT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// True only after this boot CPU located its own redistributor and enabled
/// the timer PPI on it. When false the timer must not arm: an unmatched
/// distributor cannot deliver the tick, and writing frame 0 would target
/// another PE's banked state.
static TIMER_READY: AtomicBool = AtomicBool::new(false);
/// True only after the distributor accepted `ARE_NS` and `EnableGrp1NS` and
/// both bits read back. A per-CPU init must not enable a PPI through a
/// distributor that is still in legacy mode.
static DISTRIBUTOR_READY: AtomicBool = AtomicBool::new(false);

/// Bounded spins for redistributor wake-up and distributor RWP drain. These
/// are deliberately generous (milliseconds of CPU time) but finite: a
/// missing clock or an unpopulated frame must end in a fail-closed boot, not
/// a hang that hides the failure.
const WAKER_SPIN_LIMIT: u32 = 1_000_000;
const RWP_SPIN_LIMIT: u32 = 1_000_000;

/// Installs the addresses and INTIDs proved by the live FDT. `stride` is the
/// `redistributor-stride` property (0x20000 when absent).
pub fn configure(
    distributor_pa: usize,
    redistributor_pa: usize,
    stride: usize,
    redistributor_size: usize,
    timer: u32,
    uart: u32,
) {
    use core::sync::atomic::Ordering;
    let stride = if stride == 0 { 0x2_0000 } else { stride };
    GICD_BASE_PA.store(distributor_pa, Ordering::Relaxed);
    GICR_BASE_PA.store(redistributor_pa, Ordering::Relaxed);
    GICR_STRIDE_RT.store(stride, Ordering::Relaxed);
    let frames = if redistributor_size == 0 {
        GICR_MAX_PE
    } else {
        (redistributor_size / stride).min(GICR_MAX_PE)
    };
    GICR_MAX_PE_RT.store(frames.max(1), Ordering::Relaxed);
    TIMER_IRQ_RT.store(timer, Ordering::Relaxed);
    UART_IRQ_RT.store(uart, Ordering::Relaxed);
}

fn ensure_configured() {
    use core::sync::atomic::Ordering;
    if GICD_BASE_PA.load(Ordering::Relaxed) != 0 {
        return;
    }
    #[cfg(feature = "fog")]
    configure(0x0F20_0000, 0x0F30_0000, 0x2_0000, 0, 18, 363);
    #[cfg(not(feature = "fog"))]
    configure(0x1710_0000, 0x1718_0000, 0x4_0000, 0, 30, 494);
}

pub fn timer_irq() -> u32 {
    ensure_configured();
    TIMER_IRQ_RT.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn uart_irq() -> u32 {
    ensure_configured();
    UART_IRQ_RT.load(core::sync::atomic::Ordering::Relaxed)
}

fn gicd() -> usize {
    crate::memory::phys_to_virt(GICD_BASE_PA.load(core::sync::atomic::Ordering::Relaxed))
}

fn gicr() -> usize {
    crate::memory::phys_to_virt(GICR_BASE_PA.load(core::sync::atomic::Ordering::Relaxed))
}

fn gicr_stride() -> usize {
    GICR_STRIDE_RT.load(core::sync::atomic::Ordering::Relaxed)
}
const GICR_IGROUPR0: usize = 0x080;
const GICR_ISENABLER0: usize = 0x100;
const GICR_IPRIORITYR: usize = 0x400;
const GICR_IGRPMODR0: usize = 0xD00;
// Read-only diagnostic offsets (no driver writes these).
const GICR_CTLR: usize = 0x000;
const GICR_IIDR: usize = 0x004;
const GICR_ICFGR0: usize = 0xC00;
const GICR_ICFGR1: usize = 0xC04;

const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// GICD_CTLR: Enable Group 1 (Non-secure). bit 0 (Group 0) stays off; this
/// kernel has no secure-world interrupts.
const GICD_CTLR_ENABLE_GRP1NS: u32 = 1 << 1;
/// Affinity routing enable (non-secure). A GICv3 left in legacy mode
/// (`ARE_NS = 0`) does not route through the redistributor: its SGI/PPI frame
/// reads as zero and PPI enables are ignored. The live Rain unit was measured
/// with `GICD_CTLR = 0`, so this bit must be set by the kernel.
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_RWP: u32 = 1 << 31;

/// Shared distributor setup: runs once on the boot CPU.
pub fn init() {
    ensure_configured();
    TIMER_READY.store(false, Ordering::Relaxed);

    // ARE_NS must be proven before any SPI configuration: this GIC ignored
    // group/config writes while it was still in legacy mode. The distributor
    // readback also gates the per-CPU PPI path.
    let distributor_ready = start_distributor();
    DISTRIBUTOR_READY.store(distributor_ready, Ordering::Relaxed);
    if distributor_ready {
        configure_uart_spi();
    }

    // The boot CPU brings up its own redistributor + interface right away.
    let ready = distributor_ready && cpu_init_inner();
    TIMER_READY.store(ready, Ordering::Relaxed);
    if !ready {
        // The tablet has no accessible UART, so its diagnostic goes to the
        // ramoops zone. Fog/QEMU have a live console by this point.
        #[cfg(feature = "tablet")]
        crate::ram_log::log("GICV3: init failed; timer PPI left disabled");
        #[cfg(not(feature = "tablet"))]
        crate::console::write_str(
            "GICV3: init failed (distributor or timer PPI readback); timer left disabled\r\n",
        );
    }
}

/// Whether the boot CPU's timer PPI was enabled on its own redistributor.
pub fn timer_available() -> bool {
    TIMER_READY.load(Ordering::Relaxed)
}

/// Per-CPU GICv3 bring-up. Returns false when the CPU cannot be given a
/// usable redistributor/CPU interface; the caller must keep it offline since
/// an AP without banked SGIs is invisible to IPI and quiescence.
pub fn cpu_init() -> bool {
    if !cpu_init_inner() {
        return false;
    }
    // `configure_ppi` gates on the timer PPI. An AP additionally needs its
    // banked SGIs; re-read the same frame so a silently dropped SGI enable
    // cannot publish a CPU that is invisible to IPI and quiescence.
    let Some(redistributor) = redistributor_for_self() else {
        return false;
    };
    let expected = (1_u32 << (RESCHEDULE_IRQ % 32)) | (1_u32 << (crate::gic::QUIESCE_IRQ % 32));
    required_bits_set(
        read32(redistributor + GICR_SGI_OFFSET + GICR_ISENABLER0),
        expected,
    )
}

/// Per-CPU GICv3 bring-up: system-register interface, own redistributor
/// (located by affinity via GICR_TYPER), banked SGI/PPI enables and the CPU
/// interface registers. Returns false when no redistributor matches, in
/// which case no redistributor or PPI is touched at all.
fn cpu_init_inner() -> bool {
    ensure_configured();
    if !DISTRIBUTOR_READY.load(Ordering::Relaxed) {
        return false;
    }
    // System-register interface for the CPU side (ICC_*_EL1), not the
    // legacy MMIO one. `isb` right after, as the spec requires.
    let sre: u64;
    unsafe {
        core::arch::asm!("mrs {sre}, icc_sre_el1", sre = out(reg) sre, options(nostack));
    }
    if sre & 1 == 0 {
        unsafe {
            core::arch::asm!(
                "msr icc_sre_el1, {sre}",
                "isb",
                sre = in(reg) sre | 1,
                options(nostack),
            );
        }
    }

    let Some(redistributor) = redistributor_for_self() else {
        return false;
    };
    if !wake_redistributor(redistributor) {
        return false;
    }
    if !configure_ppi(redistributor) {
        return false;
    }

    // CPU interface: full priority mask, EOImode 0 (one write completes the
    // interrupt), Group 1 NS signalling enabled.
    unsafe {
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

    // Read back the two architectural gates that make the CPU interface
    // usable. This proves register state only, not that a device is asserting
    // its line or that the distributor routes it.
    let sre_after: u64;
    let grp1_after: u64;
    unsafe {
        core::arch::asm!("mrs {sre}, icc_sre_el1", sre = out(reg) sre_after, options(nomem, nostack));
        core::arch::asm!(
            "mrs {grp1}, icc_igrpen1_el1",
            grp1 = out(reg) grp1_after,
            options(nomem, nostack),
        );
    }
    sre_after & 1 != 0 && grp1_after & 1 != 0
}

/// Locates this CPU's redistributor control frame by matching the MPIDR
/// affinity against GICR_TYPER. The frame index is deliberately not used:
/// redistributor order does not have to follow MPIDR affinity values.
///
/// Returns `None` when no frame matches; there is no frame-0 fallback,
/// because that frame may belong to another PE and PPI enables are banked.
fn redistributor_for_self() -> Option<usize> {
    let mpidr: u64;
    unsafe {
        core::arch::asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nostack));
    }
    let affinity = mpidr_affinity(mpidr);

    let max_pe = GICR_MAX_PE_RT.load(core::sync::atomic::Ordering::Relaxed);
    let base = gicr();
    let stride = gicr_stride();
    find_redistributor_index(affinity, max_pe, |pe| {
        // GICR_TYPER.LAST stops the search inside the policy helper: reading
        // past it is an external abort where frames are not populated.
        read64(base + pe * stride + GICR_TYPER)
    })
    .map(|pe| base + pe * stride)
}

/// Clears GICR_WAKER.ProcessorSleep and waits, with a bound, for the
/// redistributor to come up. A sleeping redistributor silently drops every
/// interrupt, including the timer; a redistributor that never reports awake
/// must leave `TIMER_READY=false` instead of spinning forever.
fn wake_redistributor(rd: usize) -> bool {
    let waker = read32(rd + GICR_WAKER);
    write32(rd + GICR_WAKER, waker & !WAKER_PROCESSOR_SLEEP);
    let awake = bounded_wait(
        || read32(rd + GICR_WAKER) & WAKER_CHILDREN_ASLEEP == 0,
        WAKER_SPIN_LIMIT,
    );
    // Both bits matter: ProcessorSleep must read back as zero and
    // ChildrenAsleep must have drained. Reading only one can accept a
    // half-woken frame.
    awake
        && read32(rd + GICR_WAKER) & WAKER_PROCESSOR_SLEEP == 0
        && read32(rd + GICR_WAKER) & WAKER_CHILDREN_ASLEEP == 0
}

/// SGI/PPI frame of THIS CPU (matched by GICR_TYPER): group 1 non-secure, low
/// priority for the timer, then enable the timer PPI and the reschedule SGI.
///
/// Returns true only when the timer PPI bit is readable back in
/// `GICR_ISENABLER0` and is not configured as Group 1 Secure
/// (`GICR_IGRPMODR0`). Enabling a PPI is a banked per-PE operation; the
/// readback is on the same frame that was just written.
///
/// `GICR_IGROUPR0` is deliberately **not** part of the gate. The live Rain
/// unit reads it as zero for PPI 18 even though the same PPI is delivered to
/// EL1 as Group 1 NS: the pre-readback image armed the timer and ticked with
/// the identical writes. On this implementation the register is not a
/// reliable readback, so the distributor's `ARE_NS`/`EnableGrp1NS` state plus
/// a real tick are the proof; failing here would reject working hardware.
fn configure_ppi(redistributor: usize) -> bool {
    let sgi = redistributor + GICR_SGI_OFFSET;
    let timer_bit = 1_u32 << (timer_irq() % 32);
    let sgi_bit = 1_u32 << (RESCHEDULE_IRQ % 32);
    let quiesce_bit = 1_u32 << (crate::gic::QUIESCE_IRQ % 32);
    let ipi_bits = sgi_bit | quiesce_bit;

    let groupr = read32(sgi + GICR_IGROUPR0) | timer_bit | ipi_bits;
    write32(sgi + GICR_IGROUPR0, groupr);
    let grpmodr = read32(sgi + GICR_IGRPMODR0) & !(timer_bit | ipi_bits);
    write32(sgi + GICR_IGRPMODR0, grpmodr);
    // The NS physical timer asserts its line level-style while
    // CNTP_CTL.IStatus is set, which is what the reset ICFGR0 value
    // describes — no config change needed for a PPI.
    write8(sgi + GICR_IPRIORITYR + timer_irq() as usize, 0xA0);
    write32(sgi + GICR_ISENABLER0, timer_bit | ipi_bits);

    // The live unit has been seen with the enable bit silently clear even
    // though the write completed. Treat the readback as the only proof.
    let enabled = read32(sgi + GICR_ISENABLER0);
    let _group = read32(sgi + GICR_IGROUPR0);
    let group_modifier = read32(sgi + GICR_IGRPMODR0);
    // `_group` is read for the diagnostic path only; see the function comment
    // for why it cannot be part of the fail-closed gate on Rain.
    required_bits_set(enabled, timer_bit) && group_modifier & timer_bit == 0
}

/// P3 diagnostic (READ-ONLY): pending/enable of the physical-timer PPI in
/// THIS PE's redistributor plus the CPU interface registers. No writes.
/// Returns (GICR_ISPENDR0 & ppi_bit, GICR_ISENABLER0 & ppi_bit,
/// ICC_CTLR_EL1, ICC_PMR_EL1).
pub fn diag_redistributor_snapshot() -> (u32, u32, u32, u32) {
    let ppi_bit = 1_u32 << (timer_irq() % 32);
    let (pend, en) = match redistributor_for_self() {
        Some(redistributor) => {
            let sgi = redistributor + GICR_SGI_OFFSET;
            (
                read32(sgi + GICR_ISPENDR0) & ppi_bit,
                read32(sgi + GICR_ISENABLER0) & ppi_bit,
            )
        }
        // No proven frame: report "not pending, not enabled" without reading
        // or writing an address that belongs to someone else.
        None => (0, 0),
    };
    let ctlr: u64;
    let pmr: u64;
    unsafe {
        core::arch::asm!("mrs {}, icc_ctlr_el1", out(reg) ctlr, options(nomem, nostack));
        core::arch::asm!("mrs {}, icc_pmr_el1", out(reg) pmr, options(nomem, nostack));
    }
    (pend, en, ctlr as u32, pmr as u32)
}

/// Read-only GICv3 state used by the framebuffer diagnostic.
///
/// Nothing here writes a register, changes a group/security bit, enables a
/// PPI or acknowledges an interrupt. When no redistributor matches this CPU,
/// the redistributor-side fields stay zero rather than reading a frame that
/// could belong to another PE.
#[derive(Clone, Copy)]
pub struct DiagGic {
    pub distributor: usize,
    pub redistributor: usize,
    pub stride: usize,
    pub frames: usize,
    /// `u32::MAX` when no frame matched this CPU.
    pub frame_index: u32,
    pub gicd_ctlr: u32,
    pub gicd_typer: u32,
    pub gicd_iidr: u32,
    pub gicd_statusr: u32,
    pub gicr_typer: u64,
    pub gicr_waker: u32,
    pub gicr_ctlr: u32,
    pub gicr_iidr: u32,
    pub gicr_igroupr0: u32,
    pub gicr_igrpmodr0: u32,
    pub gicr_isenabler0: u32,
    pub gicr_ispendr0: u32,
    pub gicr_icfgr0: u32,
    pub gicr_icfgr1: u32,
    /// `GICR_IPRIORITYR[16..20]`: the four PPIs 16-19, including both timer
    /// PPIs. Read-only and never written by the diagnostic.
    pub gicr_ipriorityr0: u32,
    /// `GICR_IPRIORITYR[0..4]`: the first four SGIs.
    pub gicr_sgi_priorityr: u32,
}

impl DiagGic {
    pub const fn empty() -> Self {
        Self {
            distributor: 0,
            redistributor: 0,
            stride: 0,
            frames: 0,
            frame_index: u32::MAX,
            gicd_ctlr: 0,
            gicd_typer: 0,
            gicd_iidr: 0,
            gicd_statusr: 0,
            gicr_typer: 0,
            gicr_waker: 0,
            gicr_ctlr: 0,
            gicr_iidr: 0,
            gicr_igroupr0: 0,
            gicr_igrpmodr0: 0,
            gicr_isenabler0: 0,
            gicr_ispendr0: 0,
            gicr_icfgr0: 0,
            gicr_icfgr1: 0,
            gicr_ipriorityr0: 0,
            gicr_sgi_priorityr: 0,
        }
    }
}

/// Snapshot of the distributor, the affinity-matched redistributor and the
/// per-PE SGI/PPI frame. Read-only; see [`DiagGic`].
pub fn diag_gic() -> DiagGic {
    ensure_configured();
    let mut state = DiagGic::empty();
    state.distributor = gicd();
    state.redistributor = gicr();
    state.stride = gicr_stride();
    state.frames = GICR_MAX_PE_RT.load(Ordering::Relaxed);
    state.gicd_ctlr = read32(gicd() + GICD_CTLR);
    state.gicd_typer = read32(gicd() + GICD_TYPER);
    state.gicd_iidr = read32(gicd() + GICD_IIDR);
    state.gicd_statusr = read32(gicd() + GICD_STATUSR);

    let Some(redistributor) = redistributor_for_self() else {
        return state;
    };
    if gicr_stride() != 0 {
        state.frame_index = ((redistributor - gicr()) / gicr_stride()) as u32;
    }
    state.gicr_typer = read64(redistributor + GICR_TYPER);
    state.gicr_waker = read32(redistributor + GICR_WAKER);
    state.gicr_ctlr = read32(redistributor + GICR_CTLR);
    state.gicr_iidr = read32(redistributor + GICR_IIDR);
    let sgi = redistributor + GICR_SGI_OFFSET;
    state.gicr_igroupr0 = read32(sgi + GICR_IGROUPR0);
    state.gicr_igrpmodr0 = read32(sgi + GICR_IGRPMODR0);
    state.gicr_isenabler0 = read32(sgi + GICR_ISENABLER0);
    state.gicr_ispendr0 = read32(sgi + GICR_ISPENDR0);
    state.gicr_icfgr0 = read32(sgi + GICR_ICFGR0);
    state.gicr_icfgr1 = read32(sgi + GICR_ICFGR1);
    state.gicr_ipriorityr0 = read32(sgi + GICR_IPRIORITYR + 16);
    state.gicr_sgi_priorityr = read32(sgi + GICR_IPRIORITYR);
    state
}

/// Signals `intid` (an SGI) to every CPU in the inner-shareability domain.
/// ICC_SGI1R_EL1 with IRM=1 broadcasts to all PEs in the domain; a
/// self-directed SGI is harmless (the handler just re-evaluates the run
/// queue), which the reschedule use tolerates.
pub fn send_sgi_all_but_self(intid: u32) {
    unsafe {
        core::arch::asm!(
            "msr icc_sgi1r_el1, {value}",
            "isb",
            value = in(reg) (1_u64 << 40) | u64::from(intid & 0x0F),
            options(nostack),
        );
    }
}

/// Signals `intid` to one PE identified by its MPIDR affinity value.
///
/// ICC_SGI1R_EL1 with IRM=0: `TargetList` holds one bit per Aff0 (values
/// 0..15), and Aff1..Aff3 live in their fields. A higher Aff0 cannot be
/// addressed by a single register write without the range selector, which
/// this kernel does not use.
pub fn send_sgi_to(mpidr: u64, intid: u32) {
    let aff0 = (mpidr & 0xFF) as u32;
    if aff0 >= 16 {
        return;
    }
    let value = (1_u64 << aff0)
        | (((mpidr >> 8) & 0xFF) << 16)
        | (u64::from(intid & 0x0F) << 24)
        | (((mpidr >> 16) & 0xFF) << 32)
        | (((mpidr >> 32) & 0xFF) << 40);
    unsafe {
        core::arch::asm!(
            "msr icc_sgi1r_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nostack),
        );
    }
}

/// Routes and classifies the UART SPI in GICD but does not enable it (see the
/// module doc): geni_uart is polling TX-only, RX IRQ support comes later.
fn configure_uart_spi() {
    let irq = uart_irq();
    let word = 1_u32 << (irq % 32);
    let offset = (irq as usize / 32) * 4;

    write32(
        gicd() + GICD_IGROUPR + offset,
        read32(gicd() + GICD_IGROUPR + offset) | word,
    );
    write32(
        gicd() + GICD_IGRPMODR + offset,
        read32(gicd() + GICD_IGRPMODR + offset) & !word,
    );
    // Level-triggered: clear the two config bits for SPI 462.
    let config_offset = GICD_ICFGR + (irq as usize / 16) * 4;
    let config_shift = (irq as usize % 16) * 2;
    write32(
        gicd() + config_offset,
        read32(gicd() + config_offset) & !(0b11_u32 << config_shift),
    );
    let priority_offset = GICD_IPRIORITYR + (irq as usize / 4) * 4;
    let priority_shift = (irq as usize % 4) * 8;
    write32(
        gicd() + priority_offset,
        (read32(gicd() + priority_offset) & !(0xFF_u32 << priority_shift))
            | (0x80_u32 << priority_shift),
    );
    // Not enabled: the GICD_ICENABLER reset state already has this bit clear.
}

fn wait_rwp() -> bool {
    bounded_wait(
        || read32(gicd() + GICD_CTLR) & GICD_CTLR_RWP == 0,
        RWP_SPIN_LIMIT,
    )
}

fn start_distributor() -> bool {
    // The live unit reports GICD_CTLR = 0 after the bootloader, so ARE_NS is
    // clear and the distributor is still in legacy mode. Affinity routing must
    // be enabled first, with RWP drained, before EnableGrp1NS is accepted: a
    // combined write was ignored on this unit and left the redistributor PPI
    // frame reading all zeroes.
    let mut control = read32(gicd() + GICD_CTLR);
    if control & GICD_CTLR_ARE_NS == 0 {
        write32(gicd() + GICD_CTLR, control | GICD_CTLR_ARE_NS);
        if !wait_rwp() {
            return false;
        }
        control = read32(gicd() + GICD_CTLR);
    }
    if control & GICD_CTLR_ARE_NS == 0 {
        return false;
    }

    write32(gicd() + GICD_CTLR, control | GICD_CTLR_ENABLE_GRP1NS);
    if !wait_rwp() {
        return false;
    }

    // A write completing is not proof that the distributor accepted it.
    // Both required bits must read back before any SPI/PPI is enabled.
    required_bits_set(
        read32(gicd() + GICD_CTLR),
        GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_GRP1NS,
    )
}

pub fn enable_irq(irq: u32) {
    if irq < 32 {
        // SGI/PPI frame of THIS CPU (matched via GICR_TYPER): group routing
        // was set at init, only the enable bit changes per interrupt. Without
        // a matching frame, do nothing rather than enable it on frame 0.
        if let Some(redistributor) = redistributor_for_self() {
            let offset = redistributor + GICR_SGI_OFFSET + GICD_ISENABLER;
            write32(offset, 1_u32 << (irq % 32));
        }
        return;
    }
    let offset = GICD_ISENABLER + (irq as usize / 32) * 4;
    write32(gicd() + offset, 1_u32 << (irq % 32));
    // A dynamically enabled SPI also needs Group 1 routing; drivers that bind
    // IRQs at runtime (scheduler::bind_irq) call this after the device is set up.
    let group_offset = GICD_IGROUPR + (irq as usize / 32) * 4;
    write32(
        gicd() + group_offset,
        read32(gicd() + group_offset) | 1_u32 << (irq % 32),
    );
}

pub fn disable_irq(irq: u32) {
    let base = if irq < 32 {
        match redistributor_for_self() {
            Some(redistributor) => redistributor + GICR_SGI_OFFSET + GICD_ICENABLER,
            None => return, // no proven banked frame: nothing to disable
        }
    } else {
        gicd() + GICD_ICENABLER
    };
    let offset = base + (irq as usize / 32) * 4;
    write32(offset, 1_u32 << (irq % 32));
}

/// Masks one INTID in the register scope that owns it and confirms the
/// enable bit is clear. Returns false when no proven scope exists (reserved
/// INTID, or an SGI/PPI with no affinity-matched redistributor).
pub fn mask_irq(irq: u32) -> bool {
    let bit = 1_u32 << (irq % 32);
    let offset = (irq as usize / 32) * 4;
    let base = match irq_policy::mask_scope(irq) {
        MaskScope::Private => match redistributor_for_self() {
            Some(redistributor) => redistributor + GICR_SGI_OFFSET,
            // A banked line that cannot be tied to this PE must not be
            // written anywhere: report the failure to the caller.
            None => return false,
        },
        MaskScope::Shared => gicd(),
        MaskScope::Unavailable => return false,
    };
    write32(base + GICD_ICENABLER + offset, bit);
    read32(base + GICD_ISENABLER + offset) & bit == 0
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
/// wakes it, and an unknown INTID is masked in its proven scope. Only an
/// unknown INTID that cannot be masked reaches the exception handler, after a
/// bounded number of attempts.
pub fn handle_irq() {
    let acknowledge_register = acknowledge();
    let irq = acknowledge_register & 0x3FF;

    match irq {
        RESCHEDULE_IRQ => {
            end_interrupt(acknowledge_register);
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        crate::gic::QUIESCE_IRQ => {
            end_interrupt(acknowledge_register);
            crate::smp::quiesce_ack_and_wait();
        }
        SPURIOUS_IRQ => {}
        _ if irq == timer_irq() => {
            timer::handle_irq();
            end_interrupt(acknowledge_register);
            // Diagnostic builds arm a one-shot PSCI reset; checking it on the
            // tick keeps the deadline independent of scheduler load.
            #[cfg(feature = "fog-auto-reset")]
            crate::fog::auto_reset_poll();
            if crate::smp::irq_may_schedule() {
                scheduler::schedule();
            }
        }
        _ => {
            // Probe and wake are one kernel-lock-protected step (see gic.rs).
            // Splitting them into two windows lets another CPU unbind the
            // INTID in between, after which this path masks the line and
            // wakes nobody: the interrupt is silently lost.
            {
                let _bkl = crate::smp::BklGuard::new();
                if scheduler::irq_is_bound(irq) {
                    disable_irq(irq);
                    end_interrupt(acknowledge_register);
                    scheduler::wake_bound_irq(irq);
                    return;
                }
            }
            // No driver owns this INTID. Mask it before EOI so a level
            // source cannot re-enter immediately. If no proven scope exists,
            // `note_unknown_irq` allows only a bounded streak before the
            // exception path stops the machine.
            let masked = crate::gic::mask_irq(irq);
            end_interrupt(acknowledge_register);
            if !crate::gic::note_unknown_irq(irq, masked) {
                exceptions::silver_handle_exception(8, u64::from(irq), 0, 0);
            }
        }
    }
}

fn read32(address: usize) -> u32 {
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn read64(address: usize) -> u64 {
    unsafe { ptr::read_volatile(address as *const u64) }
}

fn write32(address: usize, value: u32) {
    unsafe { ptr::write_volatile(address as *mut u32, value) }
}

fn write8(address: usize, value: u8) {
    unsafe { ptr::write_volatile(address as *mut u8, value) }
}
