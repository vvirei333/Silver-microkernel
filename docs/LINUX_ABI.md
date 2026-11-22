# Linux ABI — Compatibility Layer

> Status: PARTIALLY IMPLEMENTED, actively developed. The layer is a kernel
> feature (`--features linux-abi`); with it off, nothing below is linked.

The Linux ABI lets unmodified AArch64 Linux binaries run on Silver. It is
not an emulator: the same scheduler, MMU and VFS serve both personalities,
and the ABI translates Linux's syscall surface onto kernel services.

## What runs today (verified in QEMU, QEMU `virt`, Cortex-A72)

| Stack | Status |
|---|---|
| Static musl binaries (test suite) | IMPLEMENTED |
| Dynamic glibc binaries (Debian trixie arm64, ld-linux + libc + libstdc++) | IMPLEMENTED |
| GLib/GIO, fontconfig, libinput clients | IMPLEMENTED |
| Mesa llvmpipe (EGL/GLES via GBM on our DRM/KMS) | IMPLEMENTED |
| OpenRC + busybox boot chain | IMPLEMENTED |
| **deniald** (Smithay compositor + embedded Flutter engine) | PARTIALLY IMPLEMENTED — engine session starts; first frame not yet produced (see STATUS.md) |
| GTK4 test apps | IMPLEMENTED (test binaries) |
| phosh/phoc | PARKED (upstream bug resolved; stack superseded by Denial) |

## Implemented syscall surface (highlights)

Full matrix lives in [STATUS.md](STATUS.md); the groups:

- **Process**: clone (threads/processes), fork, execve (+shebang
  interpretation), wait4, exit/exit_group, set_tid_address, get/setsid.
- **Memory**: mmap/munmap/mprotect/madvise (private file-backed lazy fill,
  shared memfd windows, anonymous lazy reservations), brk.
- **Descriptors**: fd tables with dup/dup2/dup3, fcntl, poll/ppoll/epoll,
  eventfd, timerfd, signalfd, pipes, memfd_create, F_ADD_SEALS.
- **Sockets**: AF_UNIX stream/seqpacket, SOCK_DGRAM, SCM_RIGHTS fd passing,
  listeners with backlogs, POSIX shm via /dev/shm.
- **Signals**: full delivery incl. group semantics, rt_sigaction/procmask,
  kill with terminating-signal group semantics.
- **UAPI devices**: DRM/KMS (modesetting, atomic with fencing properties,
  syncobjs, dumb buffers), udmabuf + DMA-BUF sync-file export (the llvmpipe
  native-fence chain), framebuffer ioctls.
- **procfs/sysfs subsets**: the nodes glibc, GLib, libdrm and Mesa probe.

## Design decisions

- **Kernel-side, not a server.** The ABI is compiled into the kernel behind
  a feature flag; a task either runs the Linux personality or the native
  one. See DESIGN_DECISIONS.md for why a userspace compatibility server was
  rejected for this stage.
- **Semantics over stubs.** Where a real errno or a real ordering matters
  (ENOTTY vs EBADF for isatty probes, STATX_MNT_ID, signalfd delivery
  before pending-bit wakes), the ABI implements real semantics rather than
  returning plausible errors. Debug prints (`OPENFAIL`, `EXECFAIL`,
  `SOCKDBG`, TRACE_RING) exist to catch the next mismatch quickly.
- **Never lie about success.** If a syscall is unsupported it returns
  ENOSYS, which let every integration bug surface as a traceable failure.

## Known gaps (open work)

- evdev input (`/dev/input`) is an empty directory today; libinput wiring is
  the next stage (see ROADMAP.md Phase 4).
- ftruncate returns EINVAL where Linux returns 0 (session-lock lockfile
  retries as a result).
- The lazy-mapping failure path under heavy region churn still kills the
  faulting task (root-caused, fix in progress — STATUS.md).
- `io_uring`, namespaces/cgroups, and multi-mount semantics are not
  implemented.
