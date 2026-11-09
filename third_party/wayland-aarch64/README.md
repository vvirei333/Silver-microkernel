# Vendored aarch64 Wayland client libraries

These files come from the Arch Linux ARM packages:

- `wayland` 1.26.0-1 (libwayland-client.so.0, headers, COPYING)
- `libffi` 3.8.0-1 (libffi.so.8)
- `wayland-protocols` 1.50-1 (xdg-shell.xml and generated client code,
  COPYING-wayland-protocols)

They are used only by the optional Linux ABI test
`linuxrun /sfs/linux-wlreal`, which runs a real libwayland client against the
in-tree mini compositor. libwayland and libffi are MIT-licensed; the
wayland-protocols XML/code is MIT-licensed. See the COPYING files.

`include/xdg-shell-client-protocol.h` and `lib/xdg-shell-protocol.c` are
generated from `xdg-shell.xml` with `wayland-scanner` and checked in so the
test build does not need the scanner.

The test image injects the two `.so` files into `/sfs` and links
`tests/linux/wlclient.c` against them.
