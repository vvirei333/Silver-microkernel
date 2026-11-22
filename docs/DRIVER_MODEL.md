# Driver and Device Model

> Status: PARTIALLY IMPLEMENTED. This document states the intended model and
> honestly marks what deviates today.

## Intended model

1. **Drivers are tasks, not kernel code.** A driver is an EL0 task holding
   capabilities for its MMIO window, IRQ lines and PCI functions
   (`capability.rs`). The kernel never trusts a driver; a driver crash is
   contained like any other task fault.
2. **Policy-gated hardware access.** Every frame allocation, IRQ request,
   DMA lease and scheduling decision passes a fail-closed policy module
   (`mem_policy`, `irq_policy`, `gicv3_policy`, `watchdog_policy`,
   `sched_policy`, `cache_policy`).
3. **Uniform device surface.** Devices appear as descriptors (fds) with
   uniform poll/read/write/ioctl semantics; native tasks get the same
   objects through capabilities.
4. **One fault domain per device**; no shared kernel driver state.

## What actually runs this way today

| Device | Placement | Status |
|---|---|---|
| Network (virtio-net + smoltcp) | net_driver device task + net_server userspace stack | IMPLEMENTED |
| Console UART (PL011, GENI) | polled in-kernel (console only) | IMPLEMENTED (deviation: in-kernel, by design — console must survive userspace loss) |
| virtio-blk | in-kernel device side, SFS above | IMPLEMENTED (deviation) |
| virtio-gpu | in-kernel DRM/KMS implementation (drm.rs) exposing the Linux UAPI | IMPLEMENTED (deviation) |
| Input (evdev) | absent; `/dev/input` is an empty dir | PLANNED |
| Audio / USB / SDHCI / eMMC | absent | PLANNED |

## Deviations, and why

The first devices (blk, gpu) run in-kernel because the Linux UAPI they must
expose (DRM atomic + fencing + syncobjs) is tightly coupled to page tables
and fault handling that live in the kernel. Moving them out is a goal, not a
regression: the plan is to keep the UAPI translation in-kernel and push the
hardware-facing half into driver tasks once the driver-process IPC path is
proven at net-driver scale (it already is for net).

## Device enumeration for Linux tasks

Linux userspace expects udev-ish metadata. Today the kernel synthesizes the
minimal `/sys` tree (DRM descriptions) and a static `/dev`. A devfs-style
enumeration service and evdev wiring are the next stage (ROADMAP Phase 4).

## FreeBSD-inspired directions (analysis, not adoption)

Some BSD engineering ideas are being evaluated for adaptation; see
DESIGN_DECISIONS.md. Nothing is copied, and no BSD code is vendored.
