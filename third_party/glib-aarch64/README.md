# Vendored aarch64 GLib for the Linux ABI test

Extracted from Arch Linux ARM packages:

- `glib2` 2.90.1-1 (`libglib-2.0.so.0`, `libgobject-2.0.so.0`,
  `libgio-2.0.so.0`, `libgmodule-2.0.so.0`, headers)
- `pcre2` 10.49-1 (`libpcre2-8.so.0`)
- `zlib` 1.3.2-4 (`libz.so.1`)
- `util-linux-libs` 2.42.4-1 (`libmount.so.1`, `libblkid.so.1`)
- `systemd-libs` 262.1-1 (`libsystemd.so.0`)
- `libgcc_s.so.1` from the AArch64 cross toolchain

GLib is LGPL-2.1-or-later (see `COPYING`); PCRE2, zlib, util-linux and
systemd carry their own licenses in `COPYING-*`.

`tests/linux/glib.c` and `tests/linux/gio.c` are built against these headers
and linked dynamically: the guest runs them with `/sfs/ld-linux-aarch64.so.1`
and an `$ORIGIN` rpath.
