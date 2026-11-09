# Vendored aarch64 Wayland binaries and libraries

These files come from Arch Linux ARM packages:

- `wayland` 1.26.0-1 (libwayland-client.so.0, headers, COPYING)
- `libffi` 3.8.0-1 (libffi.so.8)
- `wayland-protocols` 1.50-1 (xdg-shell.xml and generated client code,
  COPYING-wayland-protocols)
- `wayland-utils` 1.3.0-1 (bin/wayland-info, COPYING-wayland-utils)
- `libdrm` 2.4.134-1 (libdrm.so.2, COPYING-libdrm)

They are used only by the optional Linux ABI tests:

- `linuxrun /sfs/linux-wlreal` runs a real libwayland client built against
  the vendored headers/libraries.
- `linuxrun /sfs/linux-wlinfo` execs the unmodified stock `wayland-info`
  binary against the in-tree mini compositor.

libwayland, libffi, wayland-protocols, wayland-utils and libdrm are
MIT-licensed; see the COPYING files.

`include/xdg-shell-client-protocol.h` and `lib/xdg-shell-protocol.c` are
generated from `xdg-shell.xml` with `wayland-scanner` and checked in so the
test build does not need the scanner. The same applies to
`xdg-decoration-client-protocol.h`/`xdg-decoration-protocol.c` (from
`xdg-decoration-unstable-v1.xml`) and
`viewporter-client-protocol.h`/`viewporter-protocol.c` (from
`viewporter.xml`).

The test image injects the libraries into `/sfs`; `wayland-info` is patched
with patchelf at test-build time to use `/sfs/ld-linux-aarch64.so.1` and an
`$ORIGIN` rpath.
