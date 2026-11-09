# Vendored aarch64 GLib for the Linux ABI test

Extracted from Arch Linux ARM packages:

- `glib2` 2.90.1-1 (`libglib-2.0.so.0`, `libgobject-2.0.so.0`, headers)
- `pcre2` 10.49-1 (`libpcre2-8.so.0`)

GLib is LGPL-2.1-or-later (see `COPYING`); PCRE2 is BSD-3-Clause (see
`COPYING-pcre2`).

`tests/linux/glib.c` is built against these headers and linked dynamically:
the guest runs it with `/sfs/ld-linux-aarch64.so.1` and an `$ORIGIN` rpath.
