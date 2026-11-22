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

## D6 — SFS images are immutable in the guest; writes go to scratch

Simplicity and crash safety: the kernel serves `/tmp` writes from in-kernel
buffers and never mutates the image. A persistent writable SFS is a
roadmap item and will arrive with journaling considerations, not before.

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
| Capsicum | Capability sandboxing of fds | Silver already has kernel-wide capabilities; Capsicum-style per-fd *capability mode* for Linux tasks is a natural extension | ANALYSIS (PLANNED) |
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
