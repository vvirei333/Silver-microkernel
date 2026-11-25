# Silver Microkernel — Kernel Architecture

> Status markers: **IMPLEMENTED** (verified by code and/or kernel self-tests),
> **PARTIALLY IMPLEMENTED**, **EXPERIMENTAL**, **PLANNED**.
> Primary source of truth: `src/` (single kernel crate, ~70 modules).

Silver is a no_std Rust microkernel for AArch64. Everything runs in EL0 tasks;
the kernel is the only EL1 code. The development target is QEMU `virt`
(Cortex-A72, GICv3); Raspberry Pi 5 (`Bcm2712`) is the intended first physical
board but has no hardware log yet. The parked Redmi 10C (`fog`, SM6225)
bring-up shares the same kernel through `platform.rs`.

## 1. Memory management — IMPLEMENTED

- **MMU**: AArch64 4-stage translation, ASID-tagged TTBR0 per task
  (`mmu.rs`). User image, stack (guarded, high VA window at 0xF0000000),
  heap and a ~2.9 GiB mmap window per task.
- **Page-table bookkeeping**: all user tables are registered and tagged
  (`table_registry.rs`); teardown frees every table even after partial
  failure paths.
- **Frame allocator** (`frame_allocator.rs`): tagged frames, contiguous
  runs, a per-task frame ledger with fail-closed limits (`mem_policy.rs`:
  task frame limit derived from total RAM), and a reserve pass at boot.
- **Lazy file-backed mappings**: `mmap` of image files allocates one frame
  per touched page and fills it from SFS at fault time; anonymous
  reservations (glibc arenas) allocate on first touch.
- **DMA**: bounded DMA window + leases (`memory.rs`, `driver_resources.rs`).
- The 2026-11-22 lazy-fill failure was the 1 GiB-boundary L2 walk bug, fixed
  in `6846e49`; the generic walk is covered by a boot self-test.

## 2. Scheduler and SMP — IMPLEMENTED

- Per-CPU run queues with work stealing (`scheduler/`, `smp.rs`),
  up to 8 PEs, GICv3 SGIs for IPI reschedule. The scheduler is a module
  directory: `scheduler/mod.rs` (task table, queues, spawn/wait/exit, context
  switch) and `scheduler/selftest.rs` (boot probes and stress harnesses, which
  are a child module so they can inspect private state without widening it).
- Cooperative syscall-driven scheduling plus timer preemption; per-CPU
  idle + tick handling (`timer.rs`).
- Policy layer (`sched_policy.rs`): fail-closed scheduling decisions,
  including refusals for realtime grants (deniald's latency-thread
  elevation correctly reports "unavailable" instead of being lied to).
- Wait queues, condvars, semaphores, mutexes for kernel-side waits
  (`wait_queue.rs`, `condvar.rs`, `semaphore.rs`, `mutex.rs`).
- Exit semantics: `exit_group` marks sibling threads Dead and broadcasts an
  IPI reschedule (group kill), then reaping releases stacks, tables and
  images.

## 3. Tasks, capabilities, IPC — IMPLEMENTED

- Every task owns a **capability set** (`capability.rs`). Kernel objects
  (endpoints, MMIO ranges, IRQ lines, PCI functions, console) are reached
  only through capabilities.
- **IPC**: synchronous channel IPC with ownership transfer of memory
  (`ipc.rs`, `channel.rs`); kernel mutex/condvar for EL0 via futex-like
  primitives.
- **EL0 fault containment** (`exceptions.rs`): a faulting task is killed,
  its resources reaped, the kernel survives. Fault forensics (region dump,
  syscall ring, lazy-fill failure ring) are printed on kill.
- **Bundles** (`bundle.rs`, `bundle_sig.rs`): manifest + ELF + resources,
  SHA-256 integrity (signatures: integrity-only mode today).

## 4. Filesystem layer — IMPLEMENTED (see FILESYSTEMS.md)

- VFS over mounts (`vfs.rs`); SFS image root; synthetic procfs/sysfs trees;
  a scratch tree for `/tmp` writes; per-task open-file tables.
- SFS format: superblock + inode/bitmap/data blocks, multi-level indirect
  blocks, fsck tooling in `scripts/sfs_fsck.py`.

## 5. Device model and drivers — PARTIALLY IMPLEMENTED (see DRIVER_MODEL.md)

- In-kernel minimal device drivers: virtio-gpu (DRM/KMS surface + atomic
  UAPI), plus a synchronous 512-byte block layer that prefers DTB/PCI SDHCI
  and falls back to virtio-blk. The network device is a `net_driver` EL0 task
  and the network *stack* is a userspace process (net_server + smoltcp),
  isolated by capability.
- UART: PL011 (QEMU), GENI (fog) — console only, polled.
- Interrupts: GICv3/v2 with per-line policy checks (`irq_policy.rs`,
  `gicv3_policy.rs`).
- Missing/planned: evdev input, udev-style device enumeration for Linux
  tasks, a full driver-per-process split for gpu/blk.

## 6. Linux ABI — a kernel feature (see LINUX_ABI.md)

`linux_abi/` + `linux_fd.rs` + `linux_signal.rs` + `linux_elf.rs`
implement the Linux syscall personality: ELF loading with shebang, fd
tables, epoll/poll/timerfd/eventfd, futex, signals incl. signalfd,
AF_UNIX with SCM_RIGHTS, synthetic procfs/sysfs, DRM atomic+syncobj+udmabuf
UAPI. Gated behind the `linux-abi` cargo feature.

## 7. Diagnostics and verification

- Boot self-tests, fail closed on mismatch: MMU/ASID, SMP churn, TLB
  cross-PE probe, DMA zeroing/leases, IRQ, VFS, timer, frame-reserve
  (`src/boot/selftest.rs`; scheduler-owned probes live in
  `src/scheduler/selftest.rs`).
- `no_mangle` diagnostic rings (TRACE_RING syscall history, LAZY_FAIL/OK,
  DUMP_RING) for guest debugging; pmemsave-based post-mortem tooling in
  `tools/boot_dump.py`.
- GDB stub (`gdb.rs`) behind a feature flag.

## 8. Not implemented / not claimed

- No POSIX userspace of our own beyond the native toolbox; no journaling
  filesystem; no swap; no cgroups/containers; no network in-kernel stack
  (by design — userspace); no audio/USB drivers; SDHCI is a polled block
  back end that has been exercised under QEMU's `sdhci-pci` but not on a
  BCM2712 board; no hardware performance figures of any kind are claimed.
