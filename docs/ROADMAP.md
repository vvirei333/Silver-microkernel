# Roadmap

This is a plan, not a statement of completion. Every phase below is measured
against the code; see STATUS.md for the honest current state. No dates are
promised — the project advances when a milestone is verified, not on a
calendar.

## Phase 1 — Kernel Foundation — IMPLEMENTED (hardening ongoing)

Done and verified by boot self-tests (fail closed):

- AArch64 MMU with ASIDs, per-task address spaces, page-table registry.
- SMP scheduler (up to 8 PEs), IPI reschedule, work stealing.
- IPC channels, capability-based access, EL0 fault containment + teardown.
- Fail-closed policy layers (memory, IRQ, scheduling, cache, watchdog).
- Frame allocator with per-task ledgers; DMA windows and leases.
- GICv3/v2, timers, watchdog, UART consoles (PL011, GENI).
- Boot metrics, relocatable image, gdb stub (optional feature).

Open hardening items: lazy-fill map failure recovery (STATUS.md notes),
ftruncate errno correctness, driver-process split for gpu/blk.

## Phase 2 — Native Console / IoT — IMPLEMENTED (core), gaps open

Done: silverd, Howl, Scout, net_server (DHCP/DNS/TCP/TLS), Unix toolbox,
dynamic ELF loader, SFS + fsck, bundles with integrity.

Done (2026-11-23): `bench` (cycle-accurate fast-path timings at EL0), atomic
console handoff for foreground commands, O(1) SFS block cache, allocation-free
SFS path walk, one backend lookup per `open` (`FileSystem::metadata`),
scheduler yield that resumes a runnable task instead of parking the CPU until
the next tick, `spawn`/`bg` with text arguments plus in-shell reaping, a shell
that blocks on its mailbox instead of polling, and atomic console lines.
Numbers in [STATUS.md](STATUS.md).

Done: `silverd` supervises its fixed five-service tree with bounded restart
backoff, dependency-aware pair restarts and zombie reaping.

Open: in-OS package manager (PLANNED), a configurable service registry beyond
the fixed tree (PLANNED), real RamFs (PARTIALLY), persistent writable SFS
(PLANNED).

## Phase 3 — Linux Compatibility — PARTIALLY IMPLEMENTED (active)

Done: ELF + shebang exec, clone/fork/exec/wait, mmap family with lazy
file-backed fills, fd tables + poll/epoll/timerfd/eventfd/signalfd, futex,
signals, AF_UNIX + SCM_RIGHTS, memfd/seals, procfs/sysfs subsets, DRM
atomic/syncobj/udmabuf UAPI, glibc + GLib + Mesa stacks verified.

Open: evdev input, ftruncate/errno sweeps, lazy-fill recovery, ioctl coverage
gaps as they surface, io_uring (not started).

## Phase 4 — Mobile Environment (Denial WM) — PARTIALLY IMPLEMENTED (current focus)

Done: image builders (Alpine + Debian arm64 staging), OpenRC boot chain,
seatd handoff, deniald boots through portal → Wayland frontend → GPU
transfer → engine session start.

Current: first rendered desktop frame. Split work:

1. Kernel: recover the lazy-fill map failure (kills deniald intermittently).
2. Compositor pipeline probe: external Wayland client (`/sfs/wl-client`)
   to verify atomic commits for a client buffer.
3. Engine: Denial's kms frame clock wake path (why runner threads idle).
4. Then: input (evdev `/dev/input` + udev glue for libinput), screen
   capture client, desktop services.

## Phase 5 — Beyond QEMU (later)

- Persistent writable SFS, devfs-style enumeration.
- Driver-process split for gpu/blk.
- Hardware bring-up resumes after the QEMU desktop milestone: the Redmi 10C
  (`fog`, SM6225) port is PARKED, its kernel-side platform support
  (`platform.rs`, `fog.rs`, GENI UART, simplefb) already merged behind
  feature flags. The Legion Y700 tablet target is abandoned.
- RISC-V: considered, not started, no claims.

## Explicitly not on the roadmap

- Gaming/Steam compatibility: no plans this cycle.
- Any promise of device support, performance or compatibility that is not
  backed by a verified milestone.
