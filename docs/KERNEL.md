# Silver Microkernel — Kernel Architecture

> Status markers: **IMPLEMENTED** (verified by code and/or kernel self-tests),
> **PARTIALLY IMPLEMENTED**, **EXPERIMENTAL**, **PLANNED**.
> Primary source of truth: `src/` (single kernel crate, ~70 modules).

Silver is a no_std Rust microkernel for AArch64. Everything runs in EL0 tasks;
the kernel is the only EL1 code. Target boards: QEMU `virt` (Cortex-A72,
GICv3) and the parked Redmi 10C (`fog`, SM6225) bring-up, which shares the
same kernel through a platform abstraction (`platform.rs`).

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
- **Known issue (2026-11-22)**: the lazy fill can fail in
  `mmu::map_user_pages_at_inner` under heavy region churn (~900 regions);
  the failure currently kills the faulting task instead of reporting ENOMEM.
  Being fixed; see STATUS.md.

## 2. Scheduler and SMP — IMPLEMENTED

- Per-CPU run queues with work stealing (`scheduler.rs`, `smp.rs`),
  up to 8 PEs, GICv3 SGIs for IPI reschedule.
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

- In-kernel minimal virtio drivers: gpu (DRM/KMS surface + atomic UAPI),
  blk, net (device side). The network *stack* is a userspace process
  (net_server + smoltcp), isolated by capability.
- UART: PL011 (QEMU), GENI (fog) — console only, polled.
- Interrupts: GICv3/v2 with per-line policy checks (`irq_policy.rs`,
  `gicv3_policy.rs`).
- Missing/planned: evdev input, udev-style device enumeration for Linux
  tasks, a full driver-per-process split for gpu/blk.

## 6. Linux ABI — a kernel feature (see LINUX_ABI.md)

`linux_abi.rs` + `linux_fd.rs` + `linux_signal.rs` + `linux_elf.rs`
implement the Linux syscall personality: ELF loading with shebang, fd
tables, epoll/poll/timerfd/eventfd, futex, signals incl. signalfd,
AF_UNIX with SCM_RIGHTS, synthetic procfs/sysfs, DRM atomic+syncobj+udmabuf
UAPI. Gated behind the `linux-abi` cargo feature.

## 7. Diagnostics and verification

- Boot self-tests, fail closed on mismatch: MMU/ASID, SMP churn, TLB
  cross-PE probe, DMA zeroing/leases, IRQ, VFS, timer, frame-reserve
  (`main.rs` test section).
- `no_mangle` diagnostic rings (TRACE_RING syscall history, LAZY_FAIL/OK,
  DUMP_RING) for guest debugging; pmemsave-based post-mortem tooling in
  `tools/boot_dump.py`.
- GDB stub (`gdb.rs`) behind a feature flag.

## 8. Not implemented / not claimed

- No POSIX userspace of our own beyond the native toolbox; no journaling
  filesystem; no swap; no cgroups/containers; no network in-kernel stack
  (by design — userspace); no audio/USB/SDHCI drivers; no hardware
  performance figures of any kind are claimed.
