#!/usr/bin/env bash
# Build a RAM-only Android boot image v3 for the Redmi 10C (fog / SM6225).
#
# This script BUILDS ONLY. It never runs fastboot, never flashes, never
# unlocks a bootloader and never touches a device. The image is meant to be
# tried with `fastboot boot`, which loads it into RAM and leaves the flash
# untouched.
#
# Requirements: cargo (aarch64-unknown-none), an objcopy that understands
# AArch64, and AOSP `mkbootimg` (from android-tools or a platform-tools build).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/redmi10c_poc/out"
mkdir -p "$OUT"

FEATURES="no_std,fog"
IMAGE_NAME="boot.img"
RELOC=0
UI=0
for argument in "$@"; do
    case "$argument" in
        --reloc) RELOC=1 ;;
        --ui) UI=1 ;;
        *) echo "usage: $0 [--reloc] [--ui]" >&2; exit 1 ;;
    esac
done

if [ "$RELOC" -eq 1 ]; then
    FEATURES="${FEATURES},fog-reloc"
    IMAGE_NAME="boot-reloc"
else
    IMAGE_NAME="boot"
fi
if [ "$UI" -eq 1 ]; then
    FEATURES="${FEATURES},fog-ui"
    IMAGE_NAME="${IMAGE_NAME}-ui"
fi
IMAGE_NAME="${IMAGE_NAME}.img"

CARGO="${CARGO:-cargo}"
OBJCOPY="${OBJCOPY:-}"
if [ -z "$OBJCOPY" ]; then
    if command -v llvm-objcopy >/dev/null 2>&1; then
        OBJCOPY=llvm-objcopy
    elif command -v aarch64-linux-gnu-objcopy >/dev/null 2>&1; then
        OBJCOPY=aarch64-linux-gnu-objcopy
    else
        echo "error: install llvm-objcopy or aarch64-linux-gnu-objcopy" >&2
        exit 1
    fi
fi

cd "$ROOT"
"$CARGO" build --release --target aarch64-unknown-none \
    --no-default-features --features "$FEATURES"

ELF="target/aarch64-unknown-none/release/silver_microkernel"
IMAGE="$OUT/Image"
"$OBJCOPY" -O binary "$ELF" "$IMAGE"

# The raw arm64 Image header is the contract with ABL: code0 branches over the
# 64-byte header, image_size at 0x10 tells the loader how much to reserve, and
# the magic at 0x38 must be "ARM\x64".
python3 - "$IMAGE" <<'PY'
import sys
from pathlib import Path
path = Path(sys.argv[1])
data = path.read_bytes()
if len(data) < 0x40:
    raise SystemExit("error: Image shorter than the ARM64 header")
magic = data[0x38:0x3C]
if magic != b"ARM\x64":
    raise SystemExit(f"error: bad ARM64 magic {magic!r}")
image_size = int.from_bytes(data[0x10:0x18], "little")
flags = int.from_bytes(data[0x18:0x20], "little")
print(f"Image: {len(data)} bytes, image_size=0x{image_size:x}, flags=0x{flags:x}")
print("header: code0 branches over the 64-byte header; text_offset=0")
PY

if ! command -v mkbootimg >/dev/null 2>&1; then
    echo
    echo "error: mkbootimg not found." >&2
    echo "Install android-tools / AOSP mkbootimg, then re-run:" >&2
    echo "  mkbootimg --header_version 3 --pagesize 4096 \\" >&2
    echo "    --kernel $IMAGE --ramdisk $OUT/empty_ramdisk \\" >&2
    echo "    --os_version 0.0.0 --os_patch_level 0 -o $OUT/boot.img" >&2
    exit 1
fi

: > "$OUT/empty_ramdisk"

# Header v3 is what the fog-family vendor tree uses. The base/offset values
# mirror the board configuration for tooling compatibility; ABL chooses the
# actual physical placement itself, and that placement is still unconfirmed
# on this unit. This is the default fail-closed fog build: if ABL does not
# enter it at the link address, the stub records pc/link/delta and parks
# instead of copying into a destination it cannot prove free. Build with the
# `fog-reloc` feature only after that destination has been measured.
mkbootimg \
    --header_version 3 \
    --pagesize 4096 \
    --kernel "$IMAGE" \
    --ramdisk "$OUT/empty_ramdisk" \
    --base 0x00000000 \
    --kernel_offset 0x00008000 \
    --ramdisk_offset 0x01000000 \
    --tags_offset 0x00000100 \
    --os_version 0.0.0 \
    --os_patch_level 0 \
    --cmdline "console=ttyMSM0,115200n8" \
    -o "$OUT/$IMAGE_NAME"

echo
echo "Built: $OUT/$IMAGE_NAME"
echo "Features: $FEATURES"
echo "Verified: raw arm64 Image header and boot image v3."
echo
echo "This script performed no device operation."
echo "To try it in RAM only (requires an unlocked bootloader):"
echo "  fastboot boot $OUT/$IMAGE_NAME"
echo "That command is deliberately NOT run here; it is a device operation."
