# Platform Configurations

WerewolfOS is one OS with one kernel. Configurations differ only in which
userspace components are assembled into the boot image. Two exist today;
none of the distribution components below are kernel dependencies.

## Console / IoT — IMPLEMENTED (primary)

The native configuration: what boots from a plain Silver image.

- Userspace: silverd init, Howl shell, Scout nameserver, net_server stack,
  native toolbox, bundles.
- Filesystem: SFS.
- Linux ABI: optional (off by default in plain images; enabling the cargo
  feature adds it).
- Targets: QEMU `virt` today; hardware ports are parked (see ROADMAP.md).
- Use cases: embedded/consoles, minimal systems, the base-OS foundation the
  other configurations build on.

Build: `tools/build_alpine_*` and plain native images; the kernel crate
without `linux-abi`.

## Mobile — PARTIALLY IMPLEMENTED (active development)

The graphical configuration: real Linux desktop applications on Silver.

- Userspace: Alpine edge boot chain (busybox + OpenRC) with a Debian trixie
  arm64 glibc runtime staged into the same SFS image
  (`tools/build_glibc_denial.py`).
- Init: OpenRC (busybox init chain), driven by a boot session script.
- Desktop: **Denial WM** (deniald — Smithay compositor with an embedded
  Flutter shell) over the kernel's DRM/KMS + Mesa llvmpipe.
- Graphics stack: GBM/EGL/GLES, explicit-fencing UAPI (syncobjs, udmabuf,
  DMA-BUF sync files) implemented in the kernel's DRM layer.
- Wayland: deniald's own frontend; external clients verified up to
  connect/registry level (see STATUS.md for the current point).
- Input: not wired yet (evdev/udev glue is the next stage, PLANNED).
- Linux ABI: required.
- Native applications keep working alongside: `linuxrun` boots the Linux
  chain from the same native image.

Why Debian + Alpine: Alpine provides the proven minimal boot chain; Debian
provides the complete arm64 glibc library universe (glibc, Mesa, GLib,
fontconfig, libinput) that desktop binaries resolve against. Both are image
components — the kernel depends on neither.

## Not a configuration

- **Gaming**: not present, not planned in this cycle. Any future Steam /
  x86-64 translation work would be a separate experimental track and would
  be documented as such.
- **Phosh/phoc**: a former mobile desktop attempt, PARKED. Kept for history
  (`docs/ALPINE_PHOSH.md`, archived images); not a configuration.
