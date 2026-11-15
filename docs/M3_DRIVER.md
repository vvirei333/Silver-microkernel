# M3.1 — driver-task framework: first slice

Master plan M3.1: drivers are EL0 tasks; MMIO/IRQ capabilities come only from
the driver's DT node; the kernel maps MMIO only from that node; DT enumeration
binds drivers by `compatible`; an unmatched node stays unmapped (dead hardware,
not a crash).

This slice lands the **authority** half: device MMIO is no longer ambient.

## Landed

- `src/driver_resources.rs` — the per-program resource table. On QEMU:
  - HOWLER → PL011 `0x0900_0000..+0x1000`, INTID 33
  - HOWL (trusted shell) → GIC distributor `0x0800_0000..+0x10000`, used only
    to delegate to the IRQ/MMIO probes
  - NET_DRIVER → PCI memory-BAR window `0x1000_0000..+0x100_0000`
  - WOLFSER → ivshmem `0x1040_0000..+0x40_0000`
  - Fog HOWLER → the UART base/size/INTID from the live DT
- `fdt::driver_nodes` walks every enabled node, reads `compatible`, the first
  `reg` pair and `interrupts`, and returns bindable UART nodes (PL011 or
  GENI). `driver_resources::init` runs once at boot and prefers the node
  `find_uart` would use. QEMU's `-kernel` path passes no DTB (x0=0), so the
  fixed table remains; fog gets the live values.
- Spawn inserts the MMIO range as a **region-scoped self `CAP_MMIO`** and the
  INTID as **`CAP_IRQ` with `region = (intid, 1)`**.
- `map_phys`:
  - own DMA → allowed (owner)
  - foreign DMA → needs a region-scoped `CAP_DMA_SHARE` (F2)
  - everything else (device MMIO) → needs a self `CAP_MMIO` whose region
    covers the whole request
  - broad `CAP_MMIO` alone opens only the DMA path; it cannot map a device.
- `SYS_BIND_IRQ` checks `has_irq`: a node-scoped capability covers only its
  INTID. A broad `CAP_IRQ` remains only for the trusted shell/test probe.
- `SYS_MMIO_GRANT(recipient, base, size)` delegates a sub-range the caller
  already covers, only to itself or its own child. The shell uses it to give
  the `irq_orphan`/`mmio_range_confine` probes the GIC range.
- New probe `mmio_range_confine`: with the GIC range delegated, the GIC maps,
  the UART does not. Negative control: removing the range check makes the
  UART map and the probe **fails**; restoring it passes.

## Current limits

- The driver table binds **UART nodes**; PCI devices (virtio-net, ivshmem) are
  still matched by program id and use the static PCI/ivshmem windows. Generic
  PCI BAR publication is the next step.
- `driver_nodes` assumes 2/2 or 1/1 `reg` cells; a node with a different
  parent cell layout is skipped (fail closed) until the walker tracks parent
  `#address-cells`/`#size-cells`.
- The kernel does not spawn drivers from the DT yet: SilverD still uses the
  program table. Binding-by-compatible currently controls *what resources a
  spawned driver gets*, not *whether it is spawned*.
- Broad `CAP_IRQ` for the trusted shell/test probe is a deliberate exception,
  not a driver path.
- The kernel still owns virtio-blk/SFS. Moving the block driver to EL0 is the
  M3.2 storage step.

## Verification

- security **12/12** + post-suite health + 200k fuzz
- Linux ABI matrix **26/26** (reconstructed runner; harness `/tmp` was wiped)
- Phosh demo clean; `ping 10.0.2.2` through the net server OK
- `cargo test --lib` **224**
- `--no-linux` clean
