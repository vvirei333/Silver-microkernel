//! Driver MMIO/IRQ resources: which physical range and INTID each built-in
//! EL0 driver may use, bound from the live DT where one exists.
//!
//! `map_phys` still requires `CAP_MMIO`, but that right only opens the DMA
//! path now. A non-DMA device range must be covered by a region-scoped
//! `CAP_MMIO` capability. `SYS_BIND_IRQ` likewise requires a region-scoped
//! `CAP_IRQ` (or a broad right held by the trusted shell/test probe).
//!
//! `init` runs once at boot. A live DTB binds the console UART by
//! `compatible` (`arm,pl011` or `qcom,geni-*`); QEMU without a DTB keeps the
//! fixed PL011 address. A missing or disabled node leaves the driver
//! unbound (dead hardware, not a crash).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::fdt::{DriverKind, DriverNode, MAX_DRIVER_NODES};
use crate::mutex::Mutex;
use crate::platform::{self, Kind};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Range {
    pub base: u64,
    pub size: u64,
}

const QEMU_UART: Range = Range { base: 0x0900_0000, size: 0x1000 };
const QEMU_UART_IRQ: u32 = 33;
const QEMU_GIC: Range = Range { base: 0x0800_0000, size: 0x1_0000 };
/// PCI ECAM is reached through `SYS_PCI_*`, not `map_phys`; this is the
/// memory-BAR window the virtio driver assigns and maps (0x1010_0000,
/// 0x1020_0000 + bar * 64 KiB).
const QEMU_PCI_MMIO: Range = Range { base: 0x1000_0000, size: 0x0100_0000 };

static UART_BASE: AtomicU64 = AtomicU64::new(0);
static UART_SIZE: AtomicU64 = AtomicU64::new(0);
/// Zero means the DT did not describe an interrupt for the UART.
static UART_IRQ: AtomicU32 = AtomicU32::new(0);
/// Which driver the bound UART node needs: 1 = PL011, 2 = GENI.
static UART_KIND: AtomicU8 = AtomicU8::new(0);
/// The UART's reference clock in Hz from the tree (0 = not published). The
/// baud divisor is a function of it, so a board whose node names 48 MHz must
/// not be programmed with QEMU's 24 MHz constants.
static UART_CLK: AtomicU32 = AtomicU32::new(0);

const UART_KIND_PL011: u8 = 1;
const UART_KIND_GENI: u8 = 2;

/// GIC distributor base/size bound from the live tree (0 when absent).
static GIC_BASE: AtomicU64 = AtomicU64::new(0);
static GIC_SIZE: AtomicU64 = AtomicU64::new(0);

const MAX_MMIO_CLAIMS: usize = 16;

#[derive(Copy, Clone)]
struct MmioClaim {
    owner: u64,
    base: u64,
    size: u64,
}

/// PCI function BARs claimed by live drivers. A claim is the per-function
/// authority behind `SYS_MMIO_CLAIM`: the platform window says what is
/// possible, the claim table says who owns which part. Entries are released
/// on every task teardown path.
static MMIO_CLAIMS: Mutex<[Option<MmioClaim>; MAX_MMIO_CLAIMS]> =
    Mutex::new([None; MAX_MMIO_CLAIMS]);

/// Binds the resources described by the live tree. Called once before the
/// first user task; a second call is harmless and only fills the UART slot.
pub fn init(dtb: Option<&[u8]>) {
    let Some(tree) = dtb.and_then(crate::fdt::Fdt::new) else {
        return;
    };
    let mut nodes = [DriverNode {
        kind: DriverKind::Pl011,
        mmio: crate::fdt::Region::default(),
        irq: None,
    }; MAX_DRIVER_NODES];
    let count = tree.driver_nodes(&mut nodes);
    // Prefer the node `find_uart` would use (debug GENI over plain GENI).
    let preferred = tree.find_uart().map(|uart| uart.base);
    let chosen = nodes[..count]
        .iter()
        .find(|node| preferred.map_or(true, |base| node.mmio.start == base))
        .or_else(|| nodes[..count].first());
    if let Some(node) = chosen {
        UART_BASE.store(node.mmio.start, Ordering::Release);
        UART_SIZE.store(node.mmio.size(), Ordering::Release);
        UART_KIND.store(
            match node.kind {
                DriverKind::Pl011 => UART_KIND_PL011,
                DriverKind::GeniUart => UART_KIND_GENI,
            },
            Ordering::Release,
        );
        if let Some(irq) = node.irq {
            UART_IRQ.store(irq, Ordering::Release);
        }
    }
    // The reference clock belongs to the same node, so take it from the same
    // lookup; only trust it when it describes the UART we actually bound.
    if let Some(uart) = tree.find_uart() {
        if uart.base == UART_BASE.load(Ordering::Acquire) {
            UART_CLK.store(uart.clock.unwrap_or(0), Ordering::Release);
        }
    }

    // The interrupt controller is a device like any other: if the tree
    // describes one, that is the range drivers may be granted.
    if let Some(gic) = tree.gic() {
        GIC_BASE.store(gic.distributor, Ordering::Release);
        let size = match gic.kind {
            crate::fdt::GicKind::V2 => 0x1000,
            crate::fdt::GicKind::V3 => 0x1_0000,
        };
        GIC_SIZE.store(size, Ordering::Release);
    }
}

/// The console UART the live tree binds, if any: driver kind, MMIO range and
/// INTID. This is the portable path — a machine is supported by describing it,
/// not by naming it in the kernel.
pub fn dtb_uart_node() -> Option<(DriverKind, Range, Option<u32>)> {
    let range = dtb_uart_range()?;
    let kind = match UART_KIND.load(Ordering::Acquire) {
        UART_KIND_PL011 => DriverKind::Pl011,
        UART_KIND_GENI => DriverKind::GeniUart,
        _ => return None,
    };
    let irq = UART_IRQ.load(Ordering::Acquire);
    Some((kind, range, (irq != 0).then_some(irq)))
}

/// The console UART range, from the tree when there is one and from the
/// machine's built-in map otherwise. `None` means this platform has no console
/// UART this kernel can drive.
pub fn uart_range() -> Option<Range> {
    dtb_uart_range().or_else(default_uart_range)
}

/// The console UART's reference clock in Hz, from the tree when the node
/// publishes one. `None` means the driver should use its documented default.
pub fn uart_clock() -> Option<u32> {
    let hz = UART_CLK.load(Ordering::Acquire);
    (hz != 0).then_some(hz)
}

/// The console UART INTID, from the tree when it names one.
pub fn uart_intid() -> Option<u32> {
    let irq = UART_IRQ.load(Ordering::Acquire);
    if irq != 0 {
        return Some(irq);
    }
    match platform::detect() {
        Kind::Qemu => Some(QEMU_UART_IRQ),
        Kind::Sm8650 => Some(crate::gic::uart_irq()),
        _ => None,
    }
}

/// The GIC distributor range: the tree's description first, then the emulated
/// machine's fixed map. A board without a tree gets nothing, which fails closed.
pub fn gic_range() -> Option<Range> {
    let base = GIC_BASE.load(Ordering::Acquire);
    let size = GIC_SIZE.load(Ordering::Acquire);
    if base != 0 && size != 0 {
        return Some(Range { base, size });
    }
    match platform::detect() {
        Kind::Qemu => Some(QEMU_GIC),
        _ => None,
    }
}

/// Built-in UART map for machines this kernel can boot without a device tree.
///
/// This is a *fallback table*, not the primary source: any tree that names a
/// UART wins. Keeping it here means the device paths above never branch on a
/// board name.
fn default_uart_range() -> Option<Range> {
    match platform::detect() {
        Kind::Qemu => Some(QEMU_UART),
        Kind::Fog => {
            let base = crate::geni_uart::physical_base();
            (base != 0).then_some(Range {
                base: base as u64,
                size: 0x1000,
            })
        }
        Kind::Sm8650 => Some(Range {
            base: 0x0089_C000,
            size: 0x1000,
        }),
        Kind::Bcm2712 | Kind::Unknown => None,
    }
}

/// Range a program may map as device MMIO, if any. The spawn path adds it as
/// a self `CAP_MMIO` region; it is not inheritable through a generic handle.
///
/// The answer depends on the *program*, not on the board: a console driver gets
/// the UART, an IRQ probe gets the interrupt controller, and everything else
/// must go through `claim_mmio`. Which addresses those are comes from the live
/// tree (`init`), with the built-in map only as a fallback.
pub fn mmio_range(program: u64) -> Option<Range> {
    match program {
        crate::user_program::HOWLER => uart_range(),
        // The shell holds the GIC range only to delegate it to the IRQ probe.
        crate::user_program::HOWL => gic_range(),
        // NET_DRIVER and WOLFSER obtain their BAR ranges through
        // SYS_MMIO_CLAIM after PCI enumeration; the platform only publishes
        // the window below.
        _ => None,
    }
}

/// INTID a program may bind, if the platform/DT assigned one. A missing value
/// means no node-scoped IRQ; a broad `CAP_IRQ` (trusted shell/test probe) is
/// still accepted by `bind_irq`.
pub fn irq(program: u64) -> Option<u32> {
    match program {
        crate::user_program::HOWLER => uart_intid(),
        _ => None,
    }
}

pub fn dtb_uart() -> Option<(Range, Option<u32>)> {
    let range = dtb_uart_range()?;
    let irq = UART_IRQ.load(Ordering::Acquire);
    Some((range, (irq != 0).then_some(irq)))
}

fn dtb_uart_range() -> Option<Range> {
    let base = UART_BASE.load(Ordering::Acquire);
    let size = UART_SIZE.load(Ordering::Acquire);
    (base != 0 && size != 0).then_some(Range { base, size })
}

/// Whether the platform/DT bound this driver program. SilverD uses this to
/// spawn a driver only when its node exists; an unmatched node stays dead
/// instead of producing a driver that cannot map anything.
pub fn bound(program: u64) -> bool {
    match program {
        crate::user_program::HOWLER => mmio_range(program).is_some(),
        crate::user_program::NET_DRIVER | crate::user_program::WOLFSER => {
            pci_window().is_some()
        }
        _ => false,
    }
}

/// The platform's PCI memory window, if the machine has one. A driver with
/// `CAP_PCI` may claim a sub-range for a BAR; the claim is recorded and
/// turned into a region-scoped `CAP_MMIO` capability by the syscall layer.
pub fn pci_window() -> Option<Range> {
    match platform::detect() {
        Kind::Qemu => Some(QEMU_PCI_MMIO),
        _ => None,
    }
}

/// Claims `[base, base + size)` for `owner`. Ranges outside the platform
/// window or overlapping another live owner are refused. Re-claiming the same
/// range is idempotent (a driver may map several capabilities of one BAR).
pub fn claim_mmio(owner: u64, base: u64, size: u64) -> bool {
    let Some(window) = pci_window() else {
        return false;
    };
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    let window_end = window.base.saturating_add(window.size);
    if size == 0 || base < window.base || end > window_end {
        return false;
    }
    let mut claims = MMIO_CLAIMS.lock();
    for slot in claims.iter() {
        if let Some(claim) = slot {
            let claim_end = claim.base.saturating_add(claim.size);
            let overlaps = base < claim_end && claim.base < end;
            if overlaps && claim.owner != owner {
                return false;
            }
            if overlaps && claim.owner == owner && claim.base == base && claim.size == size {
                return true;
            }
        }
    }
    for slot in claims.iter_mut() {
        if slot.is_none() {
            *slot = Some(MmioClaim { owner, base, size });
            return true;
        }
    }
    false
}

/// Drops every BAR claim owned by a dying task. Called from the same teardown
/// helper that masks its bound IRQ.
pub fn release_mmio_claims(owner: u64) {
    let mut claims = MMIO_CLAIMS.lock();
    for slot in claims.iter_mut() {
        if slot.is_some_and(|claim| claim.owner == owner) {
            *slot = None;
        }
    }
}
