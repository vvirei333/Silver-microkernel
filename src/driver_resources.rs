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

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
        if let Some(irq) = node.irq {
            UART_IRQ.store(irq, Ordering::Release);
        }
    }
}

/// Range a program may map as device MMIO, if any. The spawn path adds it as
/// a self `CAP_MMIO` region; it is not inheritable through a generic handle.
pub fn mmio_range(program: u64) -> Option<Range> {
    match (platform::detect(), program) {
        (Kind::Qemu, crate::user_program::HOWLER) => Some(dtb_uart_range().unwrap_or(QEMU_UART)),
        // The shell holds the GIC range only to delegate it to the IRQ probe.
        (Kind::Qemu, crate::user_program::HOWL) => Some(QEMU_GIC),
        // NET_DRIVER and WOLFSER obtain their BAR ranges through
        // SYS_MMIO_CLAIM after PCI enumeration; the platform only publishes
        // the window below.
        (Kind::Fog, crate::user_program::HOWLER) => dtb_uart_range().or_else(|| {
            let base = crate::geni_uart::physical_base();
            (base != 0).then_some(Range {
                base: base as u64,
                size: 0x1000,
            })
        }),
        // Tablet profile is parked but must not regress if booted.
        (Kind::Sm8650, crate::user_program::HOWLER) => Some(Range {
            base: 0x0089_C000,
            size: 0x1000,
        }),
        _ => None,
    }
}

/// INTID a program may bind, if the platform/DT assigned one. A missing value
/// means no node-scoped IRQ; a broad `CAP_IRQ` (trusted shell/test probe) is
/// still accepted by `bind_irq`.
pub fn irq(program: u64) -> Option<u32> {
    match (platform::detect(), program) {
        (Kind::Qemu, crate::user_program::HOWLER) => {
            let from_dt = UART_IRQ.load(Ordering::Acquire);
            Some(if from_dt != 0 { from_dt } else { QEMU_UART_IRQ })
        }
        (Kind::Fog, crate::user_program::HOWLER) => {
            let irq = UART_IRQ.load(Ordering::Acquire);
            (irq != 0).then_some(irq)
        }
        (Kind::Sm8650, crate::user_program::HOWLER) => Some(crate::gic::uart_irq()),
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
