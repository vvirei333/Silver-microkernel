# WerewolfOS 0.1.2 — “First Howl”

Release date: 2026-11-15
Tag: `werewolfos-0.1.2`

WerewolfOS is an AArch64 microkernel written in Rust with its own drivers, VFS,
network stack and shell. Linux programs run through an optional `linux-abi`
module; with the feature off, no Linux loader or syscall code is linked into
the kernel.

This release is the first one that ships a **real desktop**: Phosh 0.57 running
on phoc 0.57 over the Linux ABI, rendered by the kernel's software path and
published into a host window. It also closes the M0 blockers from the master
plan (see `docs/MASTER_PLAN.md`).

## What is in the box

| Artifact | Description |
|---|---|
| `assets/silver-fastfetch-0.1.2.png` | Console banner (`fastfetch`) from a real boot, rendered from the serial capture in `assets/fastfetch-0.1.2.ansi` |
| `assets/demo-0.1.2.gif` | 30 s desktop demo: lock screen → unlock → app grid → Terminal → `ls /sfs` |
| `assets/demo-0.1.2.mp4` | The same recording at 1024×768, 10 fps |
| `assets/demo-0.1.2-terminal.png` | Still of the Terminal window after `ls /sfs` |
| `tools/demo.py` | Reproduces the demo and re-records it from the guest scanout |
| `tools/make_fastfetch_asset.py` | Re-renders the screenshot from a raw serial capture |
| `disk.img` | 64 MiB SFS image with the OS, libraries and the Linux ABI test suite |

The demo frames are the pixels the kernel published through the ivshmem
window, not a screen recording of the host desktop. The serial capture behind
the screenshot is the raw PL011 byte stream.

## What works

- AArch64 microkernel: SMP, per-task address spaces, COW fork, capabilities,
  IPC, mutexes, timers, watchdog, per-task memory ledger.
- virtio-blk/net, GENI UART; SFS rootfs read on demand (256 KiB cache), writable
  RamFs at `/tmp`; `howl` shell with file/process/mount/network/health commands.
- Optional Linux ABI: static and dynamic glibc, `ld.so`, threads (`CLONE_VM`),
  futex, signals, `epoll`, `timerfd`, `eventfd`, `memfd`, `AF_UNIX` +
  `SCM_RIGHTS`, D-Bus transports, real `libwayland-client`.
- A hand-written mini compositor and a nested-phoc server that runs the real
  postmarketOS/Phosh stack (`phoc` + `phosh`) with a 1024×768 software
  scanout.
- Phosh desktop: lock screen, unlock, app grid, search, launching a GTK
  Terminal, typing and running commands.

## What does not work yet

- **No GPU acceleration.** phoc renders with the pixman renderer; GL is
  disabled. Expect software-rendering performance and CPU cost.
- **Lockscreen wallpaper is not painted** — the lock screen is a black backdrop
  with the clock and “Slide up to unlock”. Cosmetic; tracked for 0.1.3.
- The nested window does not draw a host cursor (the guest cursor is visible
  inside the scanout).
- `phoc-output-CRITICAL: Stacked surface … not in same layer` appears when the
  overview folds. The same message appears on PureOS/Phosh on real phones; it
  is cosmetic.
- Phosh takes ~10 s to become ready and the first GTK application ~15–20 s to
  launch under QEMU TCG. Later launches are faster.
- XWayland is disabled; there is no X11 application support yet.

## Reproduce

Prerequisites: Rust with the `aarch64-unknown-none` target, an AArch64 Linux
cross compiler (`aarch64-linux-gnu-gcc`), `qemu-system-aarch64`, Python 3 with
Pillow, and `ffmpeg` for the demo encode.

```bash
rustup target add aarch64-unknown-none

# Console OS, clean image
./build_and_qemu.sh

# Install the Linux ABI test programs, then boot
./build_and_qemu.sh --linux-tests

# Build without the Linux module (no foreign ELF loader in the kernel)
./build_and_qemu.sh --no-linux --no-run

# Phosh desktop (needs the Phosh image, see below)
./build_and_qemu.sh --phosh
# at the prompt: linuxstart phosh
```

The Phosh image is built by `tools/build_phosh_image.py` from an Arch Linux
ARM rootfs plus the postmarketOS `phoc`/`phosh` packages; the default path is
`~/phosh-state/disk_phosh.img` (`PHOSH_IMAGE` overrides it). The host window is
served by the repo's own proxy (`tests/linux/wayland_real.c`, `SERVE_PHOC`),
which is also what the demo recorder reads.

To reproduce the demo recording end to end:

```bash
tools/demo.py                 # boots, drives the GUI, writes assets/demo-0.1.2.*
tools/demo.py --no-record     # drive only, no encoding
```

## Verification for this release

| Check | Result |
|---|---|
| `cargo test --lib` | 222 passed |
| Linux ABI matrix (`docs/STATUS.md`) | 26/26, including `health` with 0 zombies / 0 open files / 0 DMA leaks |
| Kernel build, `no_std,linux-abi` | clean |
| Kernel build, `no_std` (`--no-linux`) | clean |
| `uname` | `Silver Kernel v0.1.2 aarch64 (WerewolfOS)` |
| Phosh desktop | lock → unlock → grid → search → Terminal → `ls /sfs` |

The matrix was run against the image shipped in this tag. Re-run it after any
change with `./build_and_qemu.sh --linux-tests` and the commands in
`docs/STATUS.md`.

## Checksums

```
SHA256 (disk.img) = 446398056e476e0c8881324997d29698d2a4f02a44697f3c147e999c68fd30a5
SHA256 (assets/demo-0.1.2.gif) = c10ed8820c6c180a92c8502aeaf3883f3b71aca91dbd384b4c7653f742ad6c1a
SHA256 (assets/silver-fastfetch-0.1.2.png) = 8ff91fd3bb8aeb06ddec7e59b555574cfad7827acb1ab842030ba530d84d885c
```

## Post-release fixes on `main`

These land after the tag and will be in 0.1.3:

- **HTTP chunked-body DoS (fixed in `5b01348`).** A response with a chunk size
  of `0xFFFFFFFFFFFFFFFF` made the network server spin forever (the parser's
  `input + 1` wrapped in release builds). 0.1.2 binaries are affected. The
  parser now clamps its cursor; the regression is the named seed of the host
  fuzzer in `tests/security/fuzz/`.
- **`rt_sigreturn` privilege escalation (fixed in `3c4e834`, critical).**
  0.1.2 accepted the user's `pstate` from a signal frame and returned to it
  with `eret`: `PSTATE.M = EL1h` executed the task's PC at EL1 with the
  task's registers (proved with `tests/security/sigreturn_pstate.c`, which
  panics the unfixed kernel with `EL1 EXCEPTION`). Non-EL0t frames are now
  rejected with SIGSEGV. **Any 0.1.2 build that runs Linux binaries should
  take this fix.**

## Security campaign M2 — closed

Tag `security-m2-complete` (on `55f557d`, after the release tag) closes the
first security phase, audited slice by slice:

- **L1 (critical):** `rt_sigreturn` accepted a user PSTATE and `eret`ed to
  EL1 with the user's PC and registers — a local privilege escalation. Fixed
  and pinned by `tests/security/sigreturn_pstate.c`; the sanitizer now keeps
  only `NZCV|DAIF` and rejects any non-AArch64-EL0t frame.
- Four DoS classes fixed: the HTTP chunk-size spin, unbounded console writes,
  mailbox pinning (per-sender quota) and orphaned IRQ teardown. L10 (EL0
  `msr daifset` without FEAT_FGT) is a documented, accepted risk on the SMP
  product (gap #10).
- Authority model tightened: target-scoped `CAP_DMA_SHARE` (F2) and
  delegation only to self or one's own child (F3), both with native EL0
  regressions (`dma_cross_read`, `grant_foreign`) verified **failing with the
  gates reverted** before passing on the fixed tree.
- Release gate: 6 guest regressions, 200k-case parser fuzz, 223 host unit
  tests, matrix 26/26 and a clean `--no-linux` build.

The full audit trail (snapshots, leads, verdicts, manifests) is in
[`docs/audit/`](docs/audit/); the contract and accepted gaps are in
[`docs/SECURITY.md`](docs/SECURITY.md).

## License and project direction

MIT — see `LICENSE`. The project vector (console OS first, Linux applications
through the compatibility layer, Phosh as the active desktop goal) is in
`docs/VECTOR.md`; the roadmap and milestones are in `docs/MASTER_PLAN.md`.
