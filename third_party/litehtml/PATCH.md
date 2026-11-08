# Vendored litehtml

Upstream: <https://github.com/litehtml/litehtml> (New BSD License).
The engine is used through `userspace/engine/` (the Wolfser surface bridge).

## Local changes

- `src/url.cpp` builds URLs and percent-encodes without `<sstream>` /
  `<iomanip>`. The stream/locale subset pulls a large amount of libstdc++ and
  is unnecessary for the four URL operations litehtml performs.
- `src/encodings.cpp`: `prescan_for_encoding()` returns UTF-8 instead of
  running the WHATWG byte-stream prescan. That prescan aborts by throwing a
  private exception; the bare-metal AArch64 unwinder (Rust
  `compiler_builtins`) cannot unwind C++ frames and calls `abort()` at the end
  of every document. Wolfser only fetches UTF-8 pages, and litehtml still
  honours a BOM and an explicit HTTP charset, so the prescan is not needed.

Everything else is upstream, built for `aarch64-none-elf` with newlib and
libstdc++ (see `userspace/engine/build.sh`).
