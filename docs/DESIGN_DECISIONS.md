# Design Decisions

Decisions with reasons. New decisions are appended; superseded ones are
marked, not deleted.

## D1 — Linux ABI lives in the kernel, not in a userspace server

A compatibility server (like Linux-side translation daemons) would add a
fault domain and IPC hop on every syscall. glibc/GLib stacks issue tens of
thousands of syscalls during startup; the ABI is compiled into the kernel
behind the `linux-abi` feature instead. Cost: kernel TCB grows when the
feature is enabled — accepted, because the ABI's translation surface is
stateless per task and uses the same checked paths as native syscalls.

## D2 — Never fake success

Unsupported syscalls return ENOSYS; unsupported capabilities return real
errors. Every integration bug so far surfaced as a traceable failure rather
than a silent wrong behavior. Debug scaffolding (OPENFAIL, EXECFAIL, SOCKDBG,
TRACE_RING, LAZY_FAIL) exists to make the next mismatch a five-minute find.

## D3 — Fail-closed policy modules for every resource

Memory frames, IRQ lines, scheduling grants, cache control, watchdog and
DMA each have a policy module. A resource is refused unless explicitly
allowed. This is why, for example, deniald's realtime-thread requests
report "unavailable" honestly instead of the kernel pretending to comply.

## D4 — Distribution components are image-level, never kernel-level

Alpine, OpenRC, Debian, Denial WM are staged into SFS images by host-side
builders. The kernel boots silverd + Howl with no distribution present.
This keeps the Console configuration meaningful on its own and prevents
dependency inversion (the kernel must not require OpenRC to boot).

## D5 — libsystemd is shimmed, not ported

Debian's libsystemd asserts deep inside helpers when systemd is absent
(isatty_safe, fds_are_same_mount). Rather than chasing every assert, the
image builder replaces libsystemd.so.0 with a freestanding 26-symbol stub
answering "no systemd available", and NOPs the two equivalent asserts
baked into Debian's libudev. Owner decision: cut systemd off instead of
emulating it ("systemd слишком много хавать" — OpenRC stays).

## D6 — SFS is persistent, write-through, and not yet crash-consistent

*Superseded decision (2026-11): the image used to be treated as immutable and
`/tmp` was the only writable place. That is no longer true.*

The streaming reader became a write-through block cache, so create, truncate,
write, mkdir and remove all reach the image (SFS keeps `/tmp` on RamFs for
scratch). Persistence is verified end to end: write under `/sfs`, reboot on the
same image, read the file back, and `scripts/sfs_fsck.py` still reports a
consistent bitmap and no cross-links.

The trade-off that replaced "writes go to RAM" is crash consistency: bitmap,
inode table and directory blocks are read-modify-written in place, so a power
loss in the middle of a multi-block operation can leave a damaged image. Two
ways out, both bigger than this release: a journal (redo log + replay on
mount) or a copy-on-write block tree. Until one lands, the honest statement is
"writes survive an orderly reboot, not a power cut".

## D7 — Software rendering first

llvmpipe with explicit fencing exercises the whole DRM/udmabuf/fence UAPI
path deterministically in QEMU. Real GPU drivers will reuse the same UAPI;
starting with them would couple kernel bring-up to a vendor stack.

## D8 — Console stays in-kernel

The UART console is polled from EL1 on purpose: it must work when every EL0
task is dead. This is a deliberate exception to the driver-is-a-task rule
(DRIVER_MODEL.md documents it).

## D9 — Diagnostics are `no_mangle` rings, not logging

Kernel diagnostics go into fixed rings (TRACE_RING, LAZY_FAIL/OK, DUMP_RING)
read from memory dumps after the fact. Serial consoles drop bytes under
load; rings do not. Guest stderr is captured to files and replayed
post-exit by the boot tooling for the same reason.

## D10 — One OS, image-level configurations

WerewolfOS is not a family of kernels: the Mobile configuration boots the
same Silver kernel and native init as the Console one, then `linuxrun`
starts the Linux chain. Configurations differ in userspace assembly only.

## D11 — FreeBSD ideas: analysis, selective adaptation

Silver studies BSD engineering, adopts ideas (not code). Nothing is vendored
without a license check. Current evaluation table:

| FreeBSD concept | What it is / solves | Adaptation to Silver | Status |
|---|---|---|---|
| Capsicum | Capability sandboxing of fds | Silver already has kernel-wide capabilities; the native ABI now has per-fd rights, monotonic `cap_rights_limit`, and an irreversible `cap_enter` that closes every path-based syscall. Linux descriptors carry the same rights through `SYS_SILVER_CAP`; `*at` calls keep working only through directory fds that retain `FD_LOOKUP`. `execveat` with `AT_EMPTY_PATH` supplies `fexecve` for VFS and memfd descriptors, so a confined process can still replace its image through a descriptor it already holds. Still missing: per-ioctl rights and a full rights audit of every driver entry point. | PARTIAL (native + Linux fd rights, capability mode) |
| Jails | OS-level environment isolation | Silver's isolation is task-granular; a jail-like grouping would need namespace-ish scoping of VFS + IPC names | ANALYSIS (not scheduled) |
| VFS/vnode | Uniform filesystem abstraction | Silver's VFS is smaller but follows the same shape (mounts over SFS + synthetic trees); vnode ops split is already implicit | IMPLEMENTED (own design, similar shape) |
| GEOM | Layered block storage | SFS-on-virtio-blk is flat today; aGeom-like layering matters only when real storage arrives | ANALYSIS (parked) |
| rc.d | Service scripting | OpenRC covers the Mobile config; a native service manager for Console is PLANNED and will take rc.d's dependency graph ideas | PLANNED |
| kqueue | Unified event queues | Our epoll/poll already unifies fds; kqueue's user events + filters could extend IPC waits | ANALYSIS |
| UMA | Zone allocator design | Frame allocator is simpler; UMA ideas (per-CPU caches) may inform it under contention | ANALYSIS |
| devfs | Device tree as filesystem | Synthetic /dev + /sys exist; enumeration service is PLANNED (DRIVER_MODEL.md) | PARTIALLY IMPLEMENTED |
| sysctl | Kernel parameter interface | No equivalent today; a read-only diagnostics surface is PLANNED | PLANNED |
| mbuf | Network buffer architecture | net_server uses smoltcp's buffers; mbuf-style zero-copy chains matter only if networking moves in-kernel (it will not, by D-policy) | ANALYSIS (rejected for now) |

## D12 — Denial WM, not Phosh

The first mobile desktop attempt (phosh/phoc) was parked after an upstream
crash was resolved and the stack proved architecturally heavier than the
goal. Denial (Smithay + Flutter engine) is the active desktop; phosh docs
remain as history.
