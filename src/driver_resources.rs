//! Driver MMIO resources: which physical range each built-in EL0 driver may
//! map, and (later) the DT node that owns it.
//!
//! `map_phys` still requires `CAP_MMIO`, but that right only opens the DMA
//! path now. A non-DMA device range must be covered by a region-scoped
//! `CAP_MMIO` capability. The spawn path inserts these ranges for built-in
//! drivers, and `SYS_MMIO_GRANT` lets a holder delegate a sub-range it
//! already owns (the shell does that for the GIC probe).
//!
//! QEMU uses fixed platform addresses. On the phone the UART base is the one
//! proved by the live FDT through `geni_uart::physical_base`; other nodes are
//! intentionally absent until the driver binds a DT node.

use crate::platform::{self, Kind};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Range {
    pub base: u64,
    pub size: u64,
}

const QEMU_UART: Range = Range { base: 0x0900_0000, size: 0x1000 };
const QEMU_GIC: Range = Range { base: 0x0800_0000, size: 0x1_0000 };
/// PCI ECAM is reached through `SYS_PCI_*`, not `map_phys`; this is the
/// memory-BAR window the virtio driver assigns and maps (0x1010_0000,
/// 0x1020_0000 + bar * 64 KiB).
const QEMU_PCI_MMIO: Range = Range { base: 0x1000_0000, size: 0x0100_0000 };
/// ivshmem BAR used by Wolfser and the demo window.
const QEMU_IVSHMEM: Range = Range { base: 0x1040_0000, size: 0x0040_0000 };

/// Range a program may map as device MMIO, if any. The spawn path adds it as
/// a self `CAP_MMIO` region; it is not inheritable through a generic handle.
pub fn mmio_range(program: u64) -> Option<Range> {
    match (platform::detect(), program) {
        (Kind::Qemu, crate::user_program::HOWLER) => Some(QEMU_UART),
        // The shell holds the GIC range only to delegate it to the IRQ probe.
        (Kind::Qemu, crate::user_program::HOWL) => Some(QEMU_GIC),
        (Kind::Qemu, crate::user_program::NET_DRIVER) => Some(QEMU_PCI_MMIO),
        (Kind::Qemu, crate::user_program::WOLFSER) => Some(QEMU_IVSHMEM),
        // The fog console runs from the kernel GENI driver; the EL0 UART task
        // is not spawned there yet, but the range is ready for M3.3.
        (Kind::Fog, crate::user_program::HOWLER) => {
            let base = crate::geni_uart::physical_base();
            (base != 0).then_some(Range {
                base: base as u64,
                size: 0x1000,
            })
        }
        // Tablet profile is parked but must not regress if booted.
        (Kind::Sm8650, crate::user_program::HOWLER) => Some(Range {
            base: 0x0089_C000,
            size: 0x1000,
        }),
        _ => None,
    }
}
