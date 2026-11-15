# M3.1 — driver-task framework: first slice

Master plan M3.1: drivers are EL0 tasks; MMIO/IRQ capabilities come only from
the driver's DT node; the kernel maps MMIO only from that node; DT enumeration
binds drivers by `compatible`; an unmatched node stays unmapped (dead hardware,
not a crash).

This slice lands the **authority** half: device MMIO is no longer ambient.

## Landed

- `src/driver_resources.rs` — the per-program resource table. On QEMU:
  - HOWLER → PL011 `0x0900_0000..+0x1000`
  - HOWL (trusted shell) → GIC distributor `0x0800_0000..+0x10000`, used only
    to delegate to the IRQ/MMIO probes
  - NET_DRIVER → PCI memory-BAR window `0x1000_0000..+0x100_0000`
  - WOLFSER → ivshmem `0x1040_0000..+0x40_0000`
  - Fog HOWLER → the UART base proved by the live FDT (`geni_uart`), once the
    EL0 UART task is spawned there
- Spawn inserts these as **region-scoped self `CAP_MMIO`** capabilities next
  to the broad `CAP_MMIO` right.
- `map_phys`:
  - own DMA → allowed (owner)
  - foreign DMA → needs a region-scoped `CAP_DMA_SHARE` (F2)
  - everything else (device MMIO) → needs a self `CAP_MMIO` whose region
    covers the whole request
  - broad `CAP_MMIO` alone opens only the DMA path; it cannot map a device.
- `SYS_MMIO_GRANT(recipient, base, size)` delegates a sub-range the caller
  already covers, only to itself or its own child. The shell uses it to give
  the `irq_orphan`/`mmio_range_confine` probes the GIC range.
- New probe `mmio_range_confine`: with the GIC range delegated, the GIC maps,
  the UART does not. Negative control: removing the range check makes the
  UART map and the probe **fails**; restoring it passes.

## Current limits

- The resource table is static/platform-derived, not yet a generic DT walk.
  The next slice is: parse `compatible` + `reg` + `interrupts` for every
  enabled node, match a driver table, and grant from that node. Fog already
  proves the UART base through the live FDT; the table is ready to consume it.
- IRQ capabilities are still `CAP_IRQ` + `SYS_BIND_IRQ`, not node-scoped. The
  next slice should carry the node's INTID in the grant, like MMIO ranges.
- The PCI memory window is one range for the driver; per-function BAR ranges
  need the kernel's PCI enumerator to publish them.
- The kernel still owns virtio-blk/SFS. Moving the block driver to EL0 is the
  M3.2 storage step.

## Verification

- security **12/12** + post-suite health + 200k fuzz
- Linux ABI matrix **26/26** (reconstructed runner; harness `/tmp` was wiped)
- Phosh demo clean; `ping 10.0.2.2` through the net server OK
- `cargo test --lib` **223**
- `--no-linux` clean
