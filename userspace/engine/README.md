# Wolfser HTML engine

This directory bridges **litehtml** (BSD-3, bundled in
`third_party/litehtml`) to the guest browser. litehtml parses HTML/CSS and
computes block/inline layout; it deliberately does not draw. `wolf_container`
implements its `document_container` callback over the 900x640 RGBA ivshmem
surface and the existing 5x7 font.

## Files

- `wolf_container.cpp` / `.h` — text, fills, borders, list markers, clipping,
  viewport/media features. Colours are packed as `0xAABBGGRR` because the host
  consumes the surface as `QImage::Format_RGBA8888` on little-endian.
- `wolf_html.cpp` — small C ABI (`wolf_html_create/draw/hover/click/destroy`)
  used by `userspace/src/bin/wolfser.rs`. It also runs the C++ static
  constructors through newlib's `__libc_init_array`.
- `wolf_syscalls.c` — newlib stubs (`_sbrk` over `SYS_MMAP`, `_write`, `_read`,
  `_fstat`, `_isatty`, `_lseek`, `_exit`, `_gettimeofday`, `_times`,
  `_init`/`_fini`). The engine heap is one 20 MiB contiguous mapping; the Rust
  allocator keeps using `SYS_BRK` in a separate arena.
- `build.sh` — cross-compiles litehtml, gumbo and the glue into
  `build/wolfhtml/libwolfhtml.a` (or `$OUT`). It caches the archive and only
  recompiles when a source is newer.

## Toolchain

litehtml needs C++17, RTTI and a hosted C library. The Linux cross compiler is
**not** used because its libc targets Linux syscalls. The supported toolchain is
the xPack GNU AArch64 Embedded GCC (newlib + libstdc++):

```bash
# 156 MiB download, extracted outside the repository
curl -L -o /tmp/xpack.tar.gz \
  https://github.com/xpack-dev-tools/aarch64-none-elf-gcc-xpack/releases/download/v15.2.1-1.1/xpack-aarch64-none-elf-gcc-15.2.1-1.1-linux-x64.tar.gz
mkdir -p ~/.local/toolchains
tar -xzf /tmp/xpack.tar.gz -C ~/.local/toolchains
```

`userspace/build.rs` finds the first
`~/.local/toolchains/xpack-aarch64-none-elf-gcc-*`; set `WOLF_ARM_TOOLCHAIN` to
an explicit prefix instead. It builds the archive and passes
`libwolfhtml.a`, `libstdc++.a`, `libm.a`, `libc.a` and `libgcc.a` to `wolfser`
only, inside one `--start-group`. Other user binaries are unchanged.

## Kernel prerequisites

The linked browser image is ~6 MiB, so the kernel was widened accordingly:

- `elf::USER_IMAGE_SIZE` is 8 MiB (was 2 MiB).
- `mmu::USER_HEAP_VA` starts at `USER_IMAGE_VA + 8 MiB` and
  `mmu::USER_MMAP_VA` at `+10 MiB`.
- `mmu::create_task_space` maps an image across up to four L3 tables; the
  extra tables are registered with the address-space root and are reclaimed by
  the existing teardown walk.
- `USER_STACK_PAGES` is 64 (256 KiB) for the layout recursion.
- The per-task mmap quota is 24 MiB; the engine heap uses 20 MiB.

## Known limits

- Reader mode (Wikipedia's `mw-content-text`, then `<main>`/`<article>`) is
  capped at 48 KiB of sanitized HTML. Tables, scripts, styles, hidden elements
  and `div`/`span` wrappers are dropped; structural tags and `href` survive.
- Full pages are rendered as-is, but only external stylesheets up to 64 KiB are
  fetched. Modern 200 KiB CSS (Wikipedia Vector) is skipped and reader mode is
  used instead.
- Images are not decoded in the engine path yet, and there is no JavaScript or
  media pipeline.
