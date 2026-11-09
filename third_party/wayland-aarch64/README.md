# Vendored aarch64 Wayland client libraries

These files come from the Arch Linux ARM packages:

- `wayland` 1.26.0-1 (libwayland-client.so.0, headers, COPYING)
- `libffi` 3.8.0-1 (libffi.so.8)

They are unmodified binaries/headers used only by the optional Linux ABI test
`linuxrun /sfs/linux-wlreal`, which runs a real libwayland client against the
in-tree mini compositor. libwayland and libffi are MIT-licensed; see COPYING.

The test image injects the two `.so` files into `/sfs` and links
`tests/linux/wlclient.c` against them.
