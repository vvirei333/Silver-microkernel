#!/usr/bin/env bash
# Build litehtml + the Wolfser container for aarch64-none-elf.
#
# The toolchain is the xPack GNU AArch64 Embedded GCC (newlib + libstdc++),
# installed outside the repository. Override WOLF_ARM_TOOLCHAIN to point at a
# different aarch64-none-elf toolchain prefix.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LITEHTML="$ROOT/third_party/litehtml"
OUT="${1:-$ROOT/build/wolfhtml}"

if [[ -n "${WOLF_ARM_TOOLCHAIN:-}" ]]; then
    TOOL="$WOLF_ARM_TOOLCHAIN"
else
    TOOL="$(ls -d "$HOME"/.local/toolchains/xpack-aarch64-none-elf-gcc-* 2>/dev/null | head -n 1)"
fi
if [[ -z "${TOOL:-}" || ! -x "$TOOL/bin/aarch64-none-elf-g++" ]]; then
    echo "wolfhtml: aarch64-none-elf-g++ not found (set WOLF_ARM_TOOLCHAIN)" >&2
    exit 2
fi

CXX="$TOOL/bin/aarch64-none-elf-g++"
CC="$TOOL/bin/aarch64-none-elf-gcc"
AR="$TOOL/bin/aarch64-none-elf-ar"

CXXFLAGS=(
    -std=c++17
    -O2
    -fno-exceptions
    -fno-threadsafe-statics
    -fno-use-cxa-atexit
    -fno-common
    -ffunction-sections
    -fdata-sections
    -DLITEHTML_NO_THREADS
    -I"$LITEHTML/include"
    -I"$LITEHTML/include/litehtml"
    -I"$LITEHTML/src"
    -I"$LITEHTML/src/gumbo/src"
    -I"$ROOT/userspace/engine"
    -I"$ROOT/third_party/stb"
)
CFLAGS=(
    -std=c99
    -O2
    -fno-common
    -ffunction-sections
    -fdata-sections
    -funwind-tables
    -I"$LITEHTML/src/gumbo/src"
    -I"$ROOT/third_party/stb"
)

mkdir -p "$OUT/obj"

# Cargo re-runs build.rs on every source change; keep the expensive 75-object
# cross build out of the inner loop when nothing relevant changed.
if [[ -f "$OUT/libwolfhtml.a" ]]; then
    newer="$(find "$LITEHTML/src" "$LITEHTML/include" "$ROOT/userspace/engine" \
        -type f -newer "$OUT/libwolfhtml.a" -print -quit 2>/dev/null || true)"
    if [[ -z "$newer" ]]; then
        echo "wolfhtml: up to date ($OUT/libwolfhtml.a)"
        exit 0
    fi
fi
rm -f "$OUT/libwolfhtml.a" "$OUT/obj"/*.o

compile_cpp() {
    local source="$1"
    local extra=()
    # The charset prescan and the createFromString wrapper are the only
    # files that still contain exception syntax. The prescan itself is
    # patched out (see third_party/litehtml/PATCH.md); the wrapper keeps a
    # catch for allocation failures.
    case "$source" in
        */encodings.cpp|*/wolf_html.cpp) extra+=(-fexceptions) ;;
    esac
    local object="$OUT/obj/$(echo "${source#"$ROOT"/}" | tr '/' '_' | sed 's/\.cpp$/.o/')"
    "$CXX" "${CXXFLAGS[@]}" "${extra[@]}" -c "$source" -o "$object"
}

compile_c() {
    local source="$1"
    local object="$OUT/obj/$(echo "${source#"$ROOT"/}" | tr '/' '_' | sed 's/\.c$/.o/')"
    "$CC" "${CFLAGS[@]}" -c "$source" -o "$object"
}

for source in "$LITEHTML"/src/*.cpp; do
    compile_cpp "$source"
done
for source in "$LITEHTML"/src/gumbo/src/*.c; do
    compile_c "$source"
done
compile_cpp "$ROOT/userspace/engine/wolf_container.cpp"
compile_cpp "$ROOT/userspace/engine/wolf_html.cpp"
compile_c "$ROOT/userspace/engine/stb_image.c"
compile_c "$ROOT/userspace/engine/wolf_syscalls.c"

"$AR" rcs "$OUT/libwolfhtml.a" "$OUT/obj"/*.o
echo "wolfhtml: $OUT/libwolfhtml.a ($(ls "$OUT/obj"/*.o | wc -l) objects)"
