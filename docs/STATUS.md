# WerewolfOS — current project status

_Last refreshed: 2026-11-15 (release 0.1.2: Phosh desktop, M0 closed)._

WerewolfOS is a native AArch64 console microkernel with its own drivers, VFS,
network stack and shell. Linux programs — including a real `libwayland-client`
and stock distro binaries — run through an **optional `linux-abi` module**.
Removing that Cargo feature leaves no Linux loader or syscall code in the
kernel binary.

## Architecture

```
native space                          optional linux-abi module
---------------------------------     -----------------------------------------
kernel: MMU, scheduler, IPC, caps     Linux ELF + ld.so, syscall ABI
drivers: virtio-blk/net, UART         fd table, threads, signals, futex
SFS rootfs loaded into RAM            AF_UNIX sockets + SCM_RIGHTS
VFS + RamFs scratch (/tmp)            memfd + MAP_SHARED, epoll/timerfd
TCP/IP (smoltcp), Howl shell          Wayland xdg-shell / D-Bus transports
```

The module is a build boundary, not a second architecture: Linux tasks use the
same scheduler, address spaces, VFS and frame allocator as native tasks.

## What works today

### Native base
- AArch64 kernel with SMP (`-smp 4` selftest), per-task address spaces, COW
  fork, wait/exit, capabilities, IPC mailboxes and mutexes.
- virtio-blk-pci + virtio-net, GENI UART, PSCI power off/reset.
- SFS rootfs read on demand through a 256 KiB block cache (streaming); the
  image no longer occupies RAM. Create/truncate/write, mkdir/remove and
  rename/replace all work on the same cache. The block tree reaches a third
  indirect level (format maximum about 1 GiB; the 64 MiB image is the real
  bound), and freeing a large file clears its bitmap in batched writes.
  Writable RamFs at `/tmp`.
- Console shell (`howl`) with file, process, mount, network and health
  commands; `linuxrun-<app>` resolves `/sfs/apps/<app>` then `/sfs/<app>` and
  supplies the Wayland/XDG environment.
- Fastfetch-style boot banner, kernel/userspace version **0.1.2**.

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
- Writable `/tmp` with `O_CREAT`/`O_EXCL`/`O_TRUNC`, `mkdirat`, `unlinkat`,
  `renameat`; `XDG_RUNTIME_DIR=/tmp`.
- `MAP_SHARED` memfd windows shared across processes and `fork`.

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
  (`SIMPLEFB TEST PASS 128x96`). Pure canvas is host-tested. No EL0 API or
  compositor yet; hardware photo is tomorrow's Redmi step.

### Phosh desktop

- The real postmarketOS stack runs on the Linux ABI: **phoc 0.57** (Wayland
  compositor) and **Phosh 0.57** (GTK3 shell), software-rendered with the
  pixman renderer (GL disabled).
- Nested 1024×768 scanout published into the host window through ivshmem; the
  repo's proxy (`tests/linux/wayland_real.c`, `SERVE_PHOC`) serves the nested
  compositor.
- Works end to end: lock screen, unlock, app grid, type-ahead search, launching
  a GTK Terminal, typing and running commands.
- Lock-screen background is painted from the same night-sky wallpaper through
  a GTK3 CSS file seeded by `phosh-serve` (0.1.3 fix; 0.1.2 had a black lock
  screen). Screenshot: `assets/lockscreen-0.1.3.png`.
- Demo: `assets/demo-0.1.2.gif`, reproducible with `tools/demo.py`.
- Known cosmetic gaps: the host cursor is not drawn, and the overview fold
  still logs the upstream `Stacked surface … not in same layer` CRITICAL (the
  same message appears on PureOS phones).

### D-Bus
- EXTERNAL auth (`SO_PEERCRED`), `Hello` method call, method return, signals.
- `NEGOTIATE_UNIX_FD` and `UNIX_FDS=1` with an `h` body: memfd passed both
  directions over `SCM_RIGHTS`.

## Limits

| Resource | Limit |
|---|---|
| SFS image | 64 MiB, 2048 inodes, read on demand (256 KiB block cache) |
| Kernel heap | 16 MiB |
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
| `linuxrun /sfs/linux-pipe` | `pipe: ping` |
| `linuxrun /sfs/linux-ipc` | `ipc ok` |
| `linuxrun /sfs/linux-signal` | `signal ok` |
| `linuxrun /sfs/linux-sighandler` | `sigframe ok` |
| `linuxrun /sfs/linux-asyncsig` | `async sig ok` |
| `linuxrun /sfs/linux-scope` | `scope ok` |
| `linuxrun /sfs/linux-threads` | `threads ok` |
| `linuxrun /sfs/linux-pthread` | `worker x` / `pthread 42` |
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
| `health` | 0 zombies, 0 open files, 0 DMA leaks |

Host-side: `cargo test --lib` → **222 tests**. Feature builds: `no_std`,
`no_std,linux-abi`, `no_std,linux-abi-trace`, `no_std,smp-selftest`; the
`--no-linux` build is clean (the switch is also a regression test for Linux
code leaking into the kernel).

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
- `src/linux_abi.rs`, `src/linux_fd.rs`, `src/linux_signal.rs`,
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

1. Several clients with **per-surface focus** (keyboard/pointer enter/leave
   driven by surface commits) and a small animation loop in the compositor.
2. **Streaming SFS** (read blocks on demand instead of loading the whole
   image) to lift the 32 MiB/RAM limit, then a larger file-tree format.
3. D-Bus `match` rules / `AddMatch`, then a real daemon or toolkit client.
4. Network base maintenance: keep-alive, TLS roots/RTC/RNG, TLS 1.2.
