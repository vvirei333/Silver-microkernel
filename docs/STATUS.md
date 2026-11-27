# WerewolfOS — Implementation Status

_Last refreshed: 2026-11-26. Verification markers: **IMPLEMENTED** (proven
by code + boot self-tests or reproducible QEMU runs), **PARTIALLY
IMPLEMENTED**, **EXPERIMENTAL**, **PLANNED**. Detailed session notes live in
`handoff.md` (working notes); this file is the public summary._

WerewolfOS is one OS on the Silver microkernel (AArch64, no_std Rust) with
configurations assembled at the image level: **Console/IoT** (native
userspace) and **Mobile** (Linux ABI + OpenRC + Denial WM). The desktop
layer is **Denial (denialwm)** — a Smithay+Flutter Wayland compositor —
replacing the parked phosh/phoc stack. See [ARCHITECTURE.md](ARCHITECTURE.md),
[KERNEL.md](KERNEL.md), [LINUX_ABI.md](LINUX_ABI.md), [ROADMAP.md](ROADMAP.md).

## Verification snapshot

| Area | Marker | Evidence |
|---|---|---|
| Kernel: MMU/ASID, SMP scheduler, IPC/capabilities, EL0 containment | IMPLEMENTED | boot self-tests, fault-containment tests |
| Performance instrumentation (`bench`), console handoff, SFS/VFS fast paths | IMPLEMENTED | `bench` rows below, 26/26 matrix, `smp-selftest` |
| Native userspace (silverd, Howl, net_server, toolbox) | IMPLEMENTED | console runs |
| SFS + VFS + scratch writes | IMPLEMENTED | VFS self-test, fsck |
| Network (virtio-net device, smoltcp userspace stack) | IMPLEMENTED | DHCP/DNS/TCP/TLS fetches |
| Linux ABI core (exec, mmap, fds, epoll, futex, signals, AF_UNIX) | IMPLEMENTED | tests/linux suite, glibc stacks boot |
| Wall clock (PL031 RTC → `CLOCK_REALTIME`) | IMPLEMENTED (QEMU) | `RTC=PL031 epoch=...` at boot; `linux-clock` reports `rt=1795700784 mono=4` |
| Mesa llvmpipe over DRM/KMS + udmabuf + syncobj fencing | IMPLEMENTED | deniald reaches GPU-transfer/TEST_ONLY stages |
| Denial WM boot to engine session start | PARTIALLY IMPLEMENTED | deniald log stages verified |
| First rendered desktop frame | IN PROGRESS — current stop | scanout buffers verified black; no post-engine atomic commits |
| Block storage (SDHCI + virtio-blk) | IMPLEMENTED (QEMU) | SFS mounts over DTB/PCI SDHCI; CSD v1/v2, CMD17/24 and a write surviving reboot; no Pi 5 board log |
| Raspberry Pi 5 (BCM2712) | PLANNED (hardware log) | platform/UART/GIC/timer/PSCI/SDHCI software paths are in the tree; no serial log from a board |
| evdev input (`/dev/input/event0..1`) | IMPLEMENTED (keyboard + absolute pointer) | two virtio-input devices, evdev ioctls (`EVIOCGVERSION/GID/GNAME/GBIT/GPROP/GKEY/GABS`) and `/dev/input` listing verified in QEMU; the real systemd 257 `libudev` enumerates both devices from `/sys/class/input` + `uevent` with `ID_INPUT=1` / `ID_SEAT=seat0` (`linuxrun /sfs/udev-test`); real libinput 1.28 with its udev backend adds a keyboard and a pointer and delivers a `KEY_A` press as `LIBINPUT_EVENT_KEYBOARD_KEY` (`linuxrun /sfs/libinput-test`) |
| Package manager (in-OS) | PLANNED | host-side builders only |
| Persistent writable SFS | IMPLEMENTED | writes survive reboot; `sfs_fsck` clean |
| SFS journaling / crash consistency | PLANNED | no journal; an interrupted write can leave stale metadata |
| Hardware ports (Redmi 10C `fog`) | PARKED | platform support merged behind feature flags |

Resolved kernel issue (2026-11-22, fix `6846e49`): the intermittent
lazy-fill failure in `mmu::map_user_pages_at_inner` under heavy region churn
was the L2 walk indexing past the end of a table when a user range crossed a
1 GiB boundary. The walk is now generic for map/unmap/mprotect/physical
lookup, and a boot self-test covers the crossing. No open MMU issue is known.

## Architecture

```
native space                          optional linux-abi module
---------------------------------     -----------------------------------------
kernel: MMU, scheduler, IPC, caps     Linux ELF + ld.so, syscall ABI
drivers: SDHCI/virtio-blk, net, UART fd table, threads, signals, futex
SFS rootfs streamed on demand         AF_UNIX sockets + SCM_RIGHTS
VFS + RamFs scratch (/tmp)            memfd + MAP_SHARED, epoll/timerfd
TCP/IP (smoltcp), Howl shell          Wayland xdg-shell / D-Bus transports
```

The module is a build boundary, not a second architecture: Linux tasks use the
same scheduler, address spaces, VFS and frame allocator as native tasks.

## What works today

### Native base
- AArch64 kernel with SMP (`-smp 4` selftest), per-task address spaces, COW
  fork, wait/exit, capabilities, IPC mailboxes and mutexes.
- DTB or PCI SDHCI block, virtio-blk-pci fallback, virtio-net, PL011/GENI UART, PSCI power off/reset.
- SFS rootfs read on demand through a 256 KiB block cache (streaming); the
  image no longer occupies RAM. Create/truncate/write, mkdir/remove and
  rename/replace all work on the same cache. The block tree reaches a third
  indirect level (format maximum about 1 GiB; the 64 MiB image is the real
  bound), and freeing a large file clears its bitmap in batched writes.
  Writable RamFs at `/tmp`.
- Console shell (`howl`) with file, process, mount, network and health
  commands; `linuxrun-<app>` resolves `/sfs/apps/<app>` then `/sfs/<app>` and
  supplies the Wayland/XDG environment.
- `silverd` is a table-driven service supervisor: five fixed services, no
  heap allocation, bounded exponential restart backoff, dependency-aware pair
  restarts (net driver/server, shell/input), and zombie reaping. The shell
  exposes `svc restart <name>` for controlled restarts.
- Fastfetch-style boot banner; the version (**1.0.3**) comes from
  `Cargo.toml` through `build.rs`, so the banner, `uname` and the Linux
  `utsname` cannot drift apart.
- ConsoleOS release bundle: `scripts/make_release.sh` builds the kernel +
  image, writes `MANIFEST`/`SHA256SUMS`, produces a tarball, and
  `scripts/release_smoke.py` boots the *packaged* artifacts as a gate
  (banner, `uname` version, `ls`/`cat` over SFS, a native utility, a Linux
  ELF, `bench`, `health`). Install guide: [INSTALL.md](INSTALL.md).
- `bench` in the shell: cycle-accurate fast-path timings for the syscall trap,
  scheduler yield, the SFS and RamFs open paths, anonymous page faults and a
  raw copy baseline. EL0 reads `CNTPCT_EL0` directly (the kernel unmasks it per
  CPU in `timer::enable_el0_counter`), so the measured window contains no clock
  syscall of its own.

### Measured fast paths (QEMU `virt`, Cortex-A72, TCG, 62.5 MHz counter)

`bench` numbers from the console image, before -> after the 2026-11-23 pass.
Absolute values are TCG-emulation costs; the ratios are what the changes
bought, and the command stays available to catch regressions.

| Path | Before | After | Change |
|---|---|---|---|
| `sleep(0)` yield | 5 028 851 ns (200/s) | 4 723 ns (211 000/s) | ~1000x: a runnable task is no longer parked in `wfi` until the next 10 ms tick |
| SFS `open`+`close` | 70 416 ns | 22 503 ns | 3.1x: O(1) direct-mapped block cache, allocation-free path walk, in-place directory-name compare, one backend lookup per open |
| SFS `open`+`read(8B)`+`close` | 103 993 ns | 30 815 ns | 3.4x |
| RamFs `open`+`close` (VFS floor) | 16 385 ns | 12 991 ns | unchanged; this is VFS + trap |
| syscall round trip | 3 226 ns | 3 024 ns | unchanged |
| anonymous page fault | 406 ns/page | 399 ns/page | unchanged |
| `memcpy` 64 KiB | 2 181 MiB/s | ~1 800-2 400 MiB/s | noise floor |

Console-image fixes in the same pass:

- **Atomic console handoff.** The shell used to `SYS_SPAWN` then
  `SYS_CONSOLE_SET`; the child is Ready immediately, so one timer tick in that
  window let a fast command (`cat` of a small file) finish with no console and
  lose its output, after which the handoff failed on a zombie. Foreground
  utilities now use `SYS_SPAWN_ARGS_CONSOLE`, which hands over the console
  inside the spawn (the caller must own the console or hold
  `CAP_CONSOLE_GRANT`, so a plain driver still cannot grab it).
- **No debug output on failed opens.** `Vfs::open` printed `VFSDBG …` plus the
  resolved path to the shared console on every rejection (console noise and a
  path leak any task could trigger); the error is returned to the caller only.
- **One backend lookup per open.** `Vfs::open` asked the backend twice
  (`is_directory`, then `len`), i.e. two full SFS path walks;
  `FileSystem::metadata` answers both in one walk (default implementation keeps
  every backend correct).
- **`spawn`/`bg` take text arguments** (`spawn ls /sfs`, `bg cat <path>`); a
  decimal argument still selects the numeric form. `spawn` hands the console
  over and returns, `bg` keeps it — which is what the help always said, but the
  two verbs ran the same code. Fire-and-forget children are reaped by the shell
  at the next command (a bounded 16-handle ring), so `ps`/`health` no longer
  accumulate `dead`/`zombie` records.
- **The shell blocks on its mailbox instead of polling.** It used to poll
  `receive_timeout(.., 0)` with a 1 ms sleep: a wakeup every tick and typing
  latency bounded by the 10 ms tick. The blocking receive is woken by the UART
  driver's first byte; the idle shell costs no wakeups.
- **Console lines are atomic.** Kernel `console::write_*` and `SYS_PRINT` lock
  per call, so a line assembled from several calls could be cut in half by
  another CPU (visible as `Spawned task ID=EXIT: task=10 …`). `console::Line`
  and `syscall::Line` assemble in a stack buffer and emit with one write.
- **Kernel heap accounting is visible.** `SYS_HEALTH` (v2) reports exact heap
  used/free/total bytes and the memory-pressure level derived from free frames.
  `health` prints both, so a leak or a session approaching the reserve is
  visible without a debugger. A failed kernel allocation now prints
  `HEAP USED/FREE/TOTAL` before the panic, instead of only the requested size.

### Linux ABI
- Static and dynamic glibc programs; real `ld.so`, PIE/`PT_INTERP`, `$ORIGIN`.
- Real **GLib 2.90 / GObject / GIO** dynamic programs and `GMainLoop`
  (vendored aarch64 libs); `linuxrun /sfs/glibtest` and
  `linuxrun /sfs/giotest`.
- ~100 syscalls: files, `brk`, `mmap`/`mprotect`/`munmap`, `pread`/`pwrite`,
  `clone`, `futex`, `rt_sigaction`/`rt_sigreturn`, `epoll`, `timerfd`,
  `eventfd`, `pipe2`, `memfd_create`, `ftruncate`/`fallocate`, `socketpair`,
  named `AF_UNIX`, `sendmsg`/`recvmsg` with `SCM_RIGHTS`, `getsockopt`
  `SO_PEERCRED`, `stat`, `getdents`, `readv`/`writev`, `uname`, rlimits,
  `getrandom`.
- Threads (`CLONE_VM`), per-thread TLS, futex join, shared thread-group fds,
  heap/mmap/image and signal dispositions; `exit_group` from any thread.
  A leader may exit before its workers: the group's page tables, descriptors
  and signal table stay live until the last member is quiescent, and a fatal
  signal retires the whole group rather than one Task. `wait4` reports a
  signal death through `WIFSIGNALED`/`WTERMSIG` (core-dump and stop statuses
  are not generated yet).
- Writable `/tmp` with `O_CREAT`/`O_EXCL`/`O_TRUNC`, `mkdirat`, `unlinkat`,
  `renameat`; `XDG_RUNTIME_DIR=/tmp`.
- `MAP_SHARED` memfd windows shared across processes and `fork`.
- `fork` copy-on-write is page-granular for ELF image runs, the user stack,
  non-lazy writable `mmap` frames and lazy anonymous/file `mmap` objects: the
  first write copies only that 4 KiB page into a private frame, while the
  shared base run or backing `VmObject` stays mapped for its other pages. A
  second fork inherits those broken pages, so a child never falls back to
  stale base data. Re-granting write with `mprotect` on a forked image or
  paged mapping records the new protection but leaves the PTE read-only, so
  the next store still goes through COW.
- A lazy mapping owns one refcounted `VmObject`; `fork` shares the object
  rather than copying touched pages, and prefix/middle `munmap` shifts a
  `base_bias` instead of rekeying the object. Object frames are released when
  the last mapping drops it, and once this mapping is the last user a write
  fault also drops object pages already shadowed by private overrides.
  While a child still shares the object those pages must stay, so reclaim is
  deferred, not per-page. Object pages are owned by the Arc rather than
  per-page frame refcounts, and the per-task ledger counts shared object
  pages in each task.
- File-backed private mappings share a weak, path-keyed `VmObject` for as
  long as any mapping is alive. Object keys are absolute file page numbers,
  so mappings at different file offsets reuse the same cached pages. Cached
  pages are mapped read-only and a store copies one page into a private
  override, leaving the cached bytes untouched. VFS writes, create/unlink,
  rename and replace drop the weak entry so the next `mmap` reads fresh
  contents; mappings that already hold the object keep their snapshot.
  Cache entries do not pin frames after the last mapping drops; a live entry
  is evicted only when the 64-entry table is full, and nothing persists across
  boot. Ledger caveat: a mapping that maps a page already cached by another
  task is charged when it first faults that page,
  but an untouched cached page is still counted by `resident_pages()` at
  teardown; `release_frames` saturates, so this remains an approximation.
- **evdev input**: a polled kernel virtio-input driver (PCI `1af4:1052`) serves
  up to two devices in PCI order: `event0` (keyboard) and `event1` (absolute
  pointer/tablet). `read(2)` returns 24-byte `struct input_event` records
  (including `SYN_REPORT`), `poll` reports `POLLIN` when the bounded
  per-device ring is non-empty, and the evdev ioctls answer capability
  queries from the device configuration (`EVIOCGVERSION`, `EVIOCGID`,
  `EVIOCGNAME`, `EVIOCGBIT`, `EVIOCGPROP`, `EVIOCGKEY`, `EVIOCGABS`). Each
  descriptor remembers `O_NONBLOCK` from `open`/`fcntl`: a non-blocking read
  returns `EAGAIN` when empty, a blocking read sleeps until the next record,
  and buffers shorter than one record are `EINVAL`. Verified
  in QEMU with `virtio-keyboard-pci` + `virtio-tablet-pci`:
  `tests/linux/input.rs` (`linuxrun /sfs/linux-input`, driven by
  `tools/input_probe.py`) prints monitor-injected key press/release pairs and
  QMP-injected `ABS_X`/`ABS_Y` events; `EVIOCGABS(ABS_X)` reports `0..32767`.
- **sysfs + udev**: `/sys/class/input/event0..1` exposes `dev`, `name`,
  `uevent`, `subsystem` and `device`; `/sys/dev/char/13:64..` resolves through
  `device`; `getdents` lists the class directory and `/run/udev/data/c13:*`
  carries `E:ID_INPUT=1`, `E:ID_SEAT=seat0`. The real systemd 257
  `libudev.so.1` (`tests/linux/udev.c`) resolves an explicit syspath and
  enumerates the input subsystem: two devices, subsystem `input`, devnode
  `/dev/input/eventN`, `ID_INPUT=1`; `scan_devices=0`. This needs correct
  `statx` `STATX_MNT_ID`, `statfs`/`fstatfs` SYSFS magic, directory fds that
  survive `openat(dirfd, ...)`, `/proc/self/fd/N` reopening, `O_NOFOLLOW`
  symlink fds with `readlinkat(fd, "")`, and ENOENT for missing readlink
  targets.
- **libinput 1.28 udev backend**: `tests/linux/libinput.c` creates a
  `libinput_udev` context, assigns `seat0` and dispatches the real library.
  Both devices are added: `QEMU Virtio Keyboard` (keyboard capability) and
  `QEMU Virtio Tablet` (pointer capability), and a monitor-injected `a`
  arrives as `LIBINPUT_EVENT_KEYBOARD_KEY` with key `30` (KEY_A), pressed.
  Three enumeration gaps had to close first:
  `/sys/dev/char/13:64` is now a symlink to `/sys/class/input/event0` (and
  `13:65` to `event1`) so `udev_device_new_from_devnum()` can match the
  enumerated syspath; the udev database carries `ID_INPUT_KEYBOARD=1` for
  the keyboard and `ID_INPUT_MOUSE=1` for the absolute tablet (same tags
  udev's `input_id` derives from the capabilities); and reopening `/` now
  passes the read-policy check even though the SFS mount is internally
  `/sfs`.
  Delivering events then exposed three kernel bugs: `EVIOCGBIT(0)` returned
  virtio selector 0 (the EV_SYN codes) instead of the supported event-type
  set, so libevdev concluded there was no `EV_KEY` and silently dropped every
  key; an empty non-blocking evdev `read` returned 0 instead of `EAGAIN`, so
  libevdev saw end-of-device; and event timestamps always had `tv_sec = 0`.
  The type set is now rebuilt from the per-type bitmaps, empty reads return
  `EAGAIN`, and timestamps come from the monotonic tick counter. The 50
  `/usr/share/libinput/*.quirks` files from Debian `libinput-bin` are staged
  in the test image (`sfs_inject.py` now creates nested destinations) and
  `tools/build_glibc_denial.py` adds `libinput-bin` to the Denial closure, so
  libinput no longer reports missing quirks data.

### Kernel log ring
`src/log.rs` mirrors every kernel console write — boot self-tests, direct
driver prints, structured entries — into a 16 KiB circular buffer. Structured entries
use `log_info!`/`log_warn!`/`log_error!`/`log_debug!`/`log_trace!` (and the
`_why!` variants) and add the context a raw boot line lacks:

```
[    0.556] INFO  vfs        src\main.rs:259  mounted /sfs (why: kernel image is readable)
[    0.594] INFO  boot       src\boot\start.rs:38  boot complete, handing control to the scheduler (why: all init milestones reached)
```

The fields are uptime, level, subsystem target, `file:line` captured by the
macro, message and an optional reason. The default threshold is `INFO`;
`DEBUG`/`TRACE` compile into the same call and stay hidden until
`log::set_level` raises it. The ring is allocation-free, masks interrupts while
its index moves, and never holds its lock across a UART write.

`dmesg` calls `SYS_LOG_DUMP` (93), which copies the ring into an EL0 buffer.
The ring records kernel output only: EL0 `print`/`write` goes through
`console::write_bytes_unlogged`, so a userspace banner or screen clear can
neither flood the ring nor erase the dump. The panic path prints the newest
1536 bytes before the stable banner, so the crash has visible history as well
as a code.

### Panic banner
Every fatal path now funnels through `src/panic.rs`, which prints one stable
first line and then only the registers the caller actually has:

```
KERNEL PANIC code=0x05 (boot assertion)
ESR_EL1=00000000DEADBEEF ELR_EL1=0000000040001234 FAR_EL1=FFFF000000000000
```

| Code | Name | Source |
|---|---|---|
| `0x01` | rust panic | `panic!`, `assert!`, explicit kernel assertions |
| `0x02` | sync exception | EL1 data/instruction abort, undefined instruction |
| `0x03` | fiq | unhandled FIQ |
| `0x04` | serror | SError interrupt |
| `0x05` | boot assertion | boot-time invariants (`kind 6`) |
| `0x06` | allocator | frame/heap allocator (`kind 7`) |
| `0x07` | unexpected irq | IRQ with no handler (`kind 8`) |
| `0x08` | self-test | boot self-test failure (`kind 9`) |
| `0x09` | power off | PSCI power-off/reset failure |
| `0xff` | unknown | an internal kind without a public code |

The code is also written to ramoops as `PANIC code=0x..`; the tablet still
writes no UART output. A Rust panic adds `panic at file:line` and
`panic msg ...` below the banner.

### Wall clock
- `src/rtc.rs` binds the tree's `arm,pl031` node, samples its data register
  once at boot and logs `RTC=PL031 epoch=...`. QEMU `virt` keeps
  `0x0901_0000` as a fallback because `-kernel` does not always pass a DTB;
  any other board without the node stays clockless instead of probing MMIO.
- `clock_gettime(CLOCK_REALTIME/CLOCK_REALTIME_COARSE)` and `gettimeofday`
  return epoch + uptime. `CLOCK_MONOTONIC` and every other id keep the 100 Hz
  tick count, so programs that poll monotonic clocks are unchanged.
- `linuxrun /sfs/linux-clock` prints `rt=... mono=... gettimeofday=...` and
  `clock ok`; a QEMU run shows `rt=1795700784`, `mono=4`,
  `gettimeofday=1795700784`.

### Wayland
- Hand-written mini compositor plus a **real `libwayland-client`**:
  - core `wl_display`/`wl_registry`/`wl_compositor` v4/`wl_shm`/`wl_surface`;
  - `wl_shm` advertises both ARGB8888 and XRGB8888;
  - `xdg_wm_base` / `xdg_surface` / `xdg_toplevel` (configure/ack), with
    maximized and fullscreen configure states;
  - `xdg_positioner` / `xdg_popup` (configure, grab, popup_done);
  - `zxdg_decoration_manager_v1` (client-side configure, set/unset mode),
    `wp_viewporter` (source/destination scaling), `wl_subcompositor`
    (subsurface position/place/sync), `zxdg_output_manager_v1` v3
    (logical position/size/name/description), `xdg_activation_v1`
    (token/activate), `wp_fractional_scale_manager_v1` (preferred 2.0 scale),
    `wp_presentation` (clock id, sync_output, presented),
    `zwp_relative_pointer_manager_v1` (relative motion),
    `zwp_pointer_constraints_v1` (locked pointer) and
    `zwp_text_input_manager_v3` (enter/commit_string/done);
  - `wl_output` v4 (geometry/mode/scale/name/description/done) and `wl_seat`
    v5 (name, pointer+keyboard), plus `wl_surface.enter` for the output;
  - `wl_keyboard.keymap` memfd over `SCM_RIGHTS`, `repeat_info`,
    `wl_pointer` fixed coords;
  - `wl_data_device_manager` clipboard: source offer, data offer,
    selection, receive and source send with an `SCM_RIGHTS` fd, across two
    clients multiplexed by the test compositor;
  - two buffers, three animated frames, `wl_callback.done` +
    `wl_buffer.release` per frame;
  - **per-surface focus**: keyboard/pointer enter on the first mapped commit,
    leave on `wl_surface.destroy`;
  - **two concurrent clients** per compositor run;
  - stock `wayland-info` from `wayland-utils` enumerates the compositor.
- Vendored aarch64 libs/headers in `third_party/wayland-aarch64` (MIT).

### Boot framebuffer (M3.2a)

- Simplefb path: `FOG_PANEL` (0x5C00_0000, 720x1650, stride 720, ARGB8888)
  paints the Redmi `cont_splash` panel; QEMU runs a 128x96 synthetic selftest
  (`SIMPLEFB TEST PASS 128x96`). Pure canvas is host-tested. The native
  simplefb path stays diagnostic; the active desktop path is Denial over the
  kernel's DRM/KMS UAPI.

### Phosh desktop (parked 2026-11-19)

- The real postmarketOS stack ran on the Linux ABI: **phoc 0.57** (Wayland
  compositor) and **Phosh 0.57** (GTK3 shell), software-rendered with the
  pixman renderer (GL disabled).
- Nested 1024×768 scanout published into the host window through ivshmem; the
  repo's proxy (`tests/linux/wayland_real.c`, `SERVE_PHOC`) serves the nested
  compositor.
- Worked end to end: lock screen, unlock, app grid, type-ahead search,
  launching a GTK Terminal, typing and running commands.
- Demo: `assets/demo-0.1.2.gif`, reproducible with `tools/demo.py`.
- **Parked**: the upstream phoc 0.55.1 heap corruption (resolved as an
  upstream bug on 2026-11-18) plus the heavy GTK stack led to the Denial
  pivot below; the images and tooling stay in the tree, nothing develops.

### Denial DE campaign (2026-11-19/20, active)

- **Denial (denialwm/denial)**: Smithay-based Wayland compositor with an
  embedded Flutter shell (`deniald`). Image
  `build/werewolf-alpine-glibc-denial.img`: Alpine edge boot chain + Debian
  trixie arm64 glibc runtime (mesa/llvmpipe, seatd, libinput, glib) + denial
  aarch64 packages. Builders: `tools/build_glibc_denial.py` +
  `tools/debian_repo.py`.
- deniald 0.6.7 boots in full-DE mode (`--flutter-bundle
  /usr/lib/denial/flutter --wayland --no-xwayland`; without the bundle it
  intentionally runs a TestOnly self-test and exits): seatd activation →
  DRM atomic discovery → **EGL Initialized / GLES 3.2 llvmpipe** → portal
  IPC listening → **Wayland socket created** → DrmDevice initializing.
- The kernel grew a large glibc-compat layer for this: signalfd4, fsync,
  rt_sigtimedwait, SOCK_SEQPACKET, an AF_NETLINK stub for libudev, inotify,
  sched_*/priority set, procfs maps/environ/cpuinfo/osrelease, mkdirat
  missing-parent→ENOENT semantics, socket-namespace stat/unlink,
  **group-kill on terminating signals** (self-kill → exit_group), and a
  complete DRM atomic UAPI (planes, properties, blobs, CLOSEFB, PRIME
  export/import, ADDFB2 with real DRM capacity semantics).
- The kernel gained a large glibc/graphics compat layer for this: ioctl
  ENOTTY-vs-EBADF semantics, STATX_MNT_ID, /proc fdinfo, signalfd4,
  inotify, AF_NETLINK stub, fsync, rt_sigtimedwait, SOCK_SEQPACKET,
  sched_*/priority, group-kill on terminating signals, a widened user VA
  window (stack at 0xF0000000, mmap ~2.9 GiB), DRM syncobj ioctls +
  DRM_CAP_SYNCOBJ, plane IN_FENCE_FD / CRTC OUT_FENCE_PTR fencing
  properties, and a /dev/udmabuf controller (llvmpipe's native fences).
- **libsystemd is replaced by a shim** in the image (26 sd_* stub symbols,
  built with zig cc): the real library asserts deep inside its helpers when
  systemd is absent; libudev.so.1 embeds the same sources statically, so the
  builder also NOPs its fds_are_same_mount assert branches.
- **Current stop** (2026-11-22): the Wayland registry handshake works — the
  external `/sfs/wl-client` probe receives the global list. deniald then
  stops dispatching: it spins level-triggered on the listener and client
  sockets and never reads the follow-up bind/shm-pool messages. The Flutter
  engine still never draws frame 1 (no post-engine atomic commits); this
  dispatch livelock is the current suspect for the missing frame clock.
  The intermittent lazy-fill kill was root-caused separately and fixed in
  `6846e49`; no recurrence in four boots since. Details in `handoff.md`
  "2026-11-22 (day)".
- Debugging method: `no_mangle` static rings (TRACE_RING syscall history,
  FAIL_RING, IOC_RING, FAULT_RING, LAZY_FAIL) read through QEMU monitor
  `pmemsave`; helper `tools/boot_dump.py` boots, dumps and replays deniald's
  stderr after exit.

### D-Bus
- EXTERNAL auth (`SO_PEERCRED`), `Hello` method call, method return, signals.
- `NEGOTIATE_UNIX_FD` and `UNIX_FDS=1` with an `h` body: memfd passed both
  directions over `SCM_RIGHTS`.

## Limits

| Resource | Limit |
|---|---|
| SFS image | 64 MiB, 2048 inodes, read on demand (256 KiB block cache) |
| Kernel heap | 16 MiB console / 32 MiB with `linux-abi` |
| Single SFS file | ~1 GiB tree / image-bound (triple indirect) |
| Main Linux executable | 32 MiB, streamed into its frames |
| Whole-file read | 8 MiB (`MAX_FILE_BYTES`; interpreter and signed bundles) |
| Anonymous mmap per task | 24 MiB |
| Tasks | 64 (zombies capped separately) |
| Linux fds per process | 256 |
| User VA | image `0x4000_0000`, heap `0x4080_0000`, mmap `0x40A0_0000+`, stack `0x7FE0_1000` |

SFS writes allocate blocks through the streaming cache (bitmap, inode table
and directory blocks are read-modify-written in place). Same-parent and
cross-directory rename (files and directories), rollback on a failed move,
and replace-over-destination are implemented. The block tree now has direct,
single, double and triple indirect levels. The Linux main executable is
streamed from the VFS into its frames, so `exec` reaches 32 MiB; whole-file
reads and the interpreter path stay capped at 8 MiB so they cannot allocate
an unbounded buffer.

## Test matrix

Run `./build_and_qemu.sh --linux-tests` once, then `tools/linux_matrix.py`
boots the release kernel, sends the commands below and checks every marker.
The same list can be run manually at `howl>`:

| Command | Expected |
|---|---|
| `linuxrun /sfs/linux-hello` | `hello from linux abi`, exit 42 |
| `linuxrun /sfs/linux-rw` | `RWed` / `data+bss ok` |
| `linuxrun /sfs/linux-fork` | `parent saw child 42` |
| `linuxrun /sfs/linux-cowfork` | `cow fork ok` |
| `linuxrun /sfs/linux-reclaim` | `reclaim ok` |
| `linuxrun /sfs/linux-mmapcache` | `mmap cache ok` |
| `linuxrun /sfs/linux-pipe` | `pipe: ping` |
| `linuxrun /sfs/linux-ipc` | `ipc ok` |
| `linuxrun /sfs/linux-signal` | `signal ok` |
| `linuxrun /sfs/linux-sighandler` | `sigframe ok` |
| `linuxrun /sfs/linux-asyncsig` | `async sig ok` |
| `linuxrun /sfs/linux-scope` | `scope ok` |
| `linuxrun /sfs/linux-threads` | `threads ok` |
| `linuxrun /sfs/linux-pthread` | `worker x` / `pthread 42` |
| `linuxrun /sfs/linux-pthread-lifetime` | `leader exiting` / `worker survived` (exit 0) |
| `linuxrun /sfs/linux-leader-exit` | `worker survived` (leader exited first) |
| `linuxrun /sfs/linux-kill-group` | `group kill signal ok` (SIGKILL status, no worker fault) |
| `linuxrun /sfs/linux-exitgroup` | `worker exiting` (main killed) |
| `linuxrun /sfs/linux-groupsig` | `group signal ok` |
| `linuxrun /sfs/linux-unixsock` | `unix socket ok` |
| `linuxrun /sfs/linux-namedsock` | `named socket ok` |
| `linuxrun /sfs/linux-memfd` | `shared memfd ok` |
| `linuxrun /sfs/linux-sockets` | `glibc socket ok` |
| `linuxrun /sfs/linux-eventloop` | `event loop ok` |
| `linuxrun /sfs/linux-scratch` | `scratch ok` |
| `linuxrun /sfs/linux-wayland` | `wayland ok` (hand-written protocol) |
| `linuxrun /sfs/linux-wlreal` | `libwayland 2 clients ok` |
| `linuxrun /sfs/linux-wlinfo` | `wayland-info ok` (stock binary) |
| `linuxrun /sfs/linux-dbus` | `dbus ok` |
| `health` | 0 zombies, 0 open files, 0 DMA leaks; heap used < total, memory normal |

Host-side: `cargo test --lib` → **272 tests**. Feature builds: `no_std`,
`no_std,linux-abi`, `no_std,linux-abi-trace`, `no_std,smp`,
`no_std,smp-selftest`, `no_std,fog`, `no_std,tablet` and
`no_std,gdb-stub`; `--no-linux` stays clean (the switch is also a regression
test for Linux code leaking into the kernel).

**Release 1.0.3 gate (2026-11-23): `scripts/release_smoke.py` 8/8 green on the
packaged artifacts** (`uname` reports 1.0.3, SFS read, native utility, Linux
ELF exit 42, `bench`, `health` 0 zombie / 0 open / 0 DMA leaks).

**Release 0.1.2 matrix run (2026-11-15): 26/26 green**, including
`health` → `zombie 0 / open 0 / leaked 0`. Two stale pieces found and fixed
while gating the release: `linux-wlreal` still expected the old
`WEREWOLF_KEYMAP` marker (the compositor now ships a real XKB keymap for
wlroots), and the shell/drm PRIME paths were not feature-gated, which broke
`--no-linux`.

## Build and run

```bash
./build_and_qemu.sh                 # release + QEMU, clean disk.img
./build_and_qemu.sh --linux-tests   # install the Linux ABI tests first
./build_and_qemu.sh --no-linux      # drop the whole Linux module
./build_and_qemu.sh --no-run        # build only
git checkout -- disk.img            # restore the committed image
```

`tools/linux-test.sh` builds and injects every freestanding and glibc test
(it needs the AArch64 cross toolchain). `disk.img` in the repository already
contains the current test image.

## Repository map

- `src/` — kernel: scheduler, MMU, VFS/SFS, drivers, net, Linux ABI.
- `src/linux_abi/`, `src/linux_fd.rs`, `src/linux_signal.rs`,
  `src/linux_elf.rs` — the optional Linux personality.
- `userspace/` — native programs, including the Howl shell.
- `tests/linux/` — freestanding and glibc Linux ABI tests.
- `tests/security/` — kernel-boundary regressions (`fd_lifetime`, …) and host
  fuzz targets; runner `tools/security_test.py --fuzz`.
- `third_party/wayland-aarch64/` — vendored Wayland/libffi/libdrm binaries,
  headers and generated xdg-shell code (MIT).
- `tools/` — test injector, SFS tools, host window viewer.
- `docs/` — architecture and audit notes; this file is the status snapshot.
- `docs/SECURITY.md` — what the kernel accepts from a task, disclosure and
  the new-syscall PR gate. `docs/audit/M2/` — the frozen entry-point snapshot
  and checklist handed to the M2 auditor.
- `handoff.md` — chronological slice log with detailed rationale.

## Next

0. **Denial dispatch / first frame**: explain why deniald stops reading a
   client after the registry handshake (epoll/eventfd level semantics are the
   current suspects), then get the Flutter engine's frame clock to issue the
   first atomic commit.
1. **Route libinput events**: the udev backend now sees both devices; feed
   `libinput_dispatch()` events into the compositor's seat instead of reading
   evdev directly, and stage the `/usr/share/libinput` quirks data.
2. **Pi 5 evidence**: capture a serial log from the board, then validate the
   BCM2712 SDHCI path with a real card. The software path is ready; no board
   claim is made until that log exists.
3. **Network base maintenance**: keep-alive, TLS roots/RNG, TLS 1.2.
4. **SFS crash consistency**: journal or copy-on-write metadata; persistence
   across reboot already works.
