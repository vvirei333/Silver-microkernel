#!/usr/bin/env bash
# Builds the tablet kernel and packs it into an Android boot image.
#
#   ./tablet/build_boot_img.sh [--v1]
#
# Output: tablet/boot_silver.img (for `fastboot boot`, nothing is flashed)
# and tablet/boot_silver_dtb.img (kernel.gz + live DTB appended, for ABLs
# that expect Image.gz-dtb).
#
# Format contract with Qualcomm ABL (SM8650):
#   * the payload must look like the original arm64 Image.gz: gzip stream
#     containing an arm64 Image whose 64-byte header has the magic "ARM\x64"
#     at 0x38 — a bare objcopy binary has neither, and ABL refuses/crashes;
#   * header v3 (like the original boot.img); offsets are fixed by v3, the
#     --base/--kernel_offset/... flags exist only for v1 (--v1 fallback);
#   * our binary is NOT self-relocating (linked at 0xA8000000), while ABL
#     trusts the Image to be relocatable — the boot stub's PC marker in the
#     pmsg ramoops zone reveals the actual load address so we can relink.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1; then
    for candidate in "$HOME/.cargo/bin" "$HOME"/.rustup/toolchains/*/bin; do
        if [ -x "$candidate/cargo" ]; then
            PATH="$candidate:$PATH"
            break
        fi
    done
fi

HEADER_VERSION=3
EXTRA_MKBOOTIMG=()
if [ "${1:-}" = "--v1" ]; then
    HEADER_VERSION=1
    EXTRA_MKBOOTIMG=(--base 0x0 --kernel_offset 0x8000 --ramdisk_offset 0x1000000
                     --tags_offset 0x100 --pagesize 4096)
fi

echo "==> Building tablet kernel (no_std,tablet)"
cargo build --release --target aarch64-unknown-none \
    --no-default-features --features "no_std,tablet"

KERNEL=target/aarch64-unknown-none/release/silver_microkernel
echo "==> Entry point (linked, must be 0xA8000000):"
llvm-readelf -h "$KERNEL" | grep Entry || aarch64-linux-gnu-readelf -h "$KERNEL" | grep Entry

echo "==> Making raw Image (the arm64 header is emitted by the boot stub itself)"
aarch64-linux-gnu-objcopy -O binary "$KERNEL" tablet/Image
ls -l tablet/Image

# E2 experiment: mirror the known-good Kubuntu/this-repo kernel header fields
# (image_size padded to exactly 4 MiB, their text_offset). Code0 stays ours
# (b past header); only the fields ABL might validate change.
python3 - <<'PYINNER'
import struct
d = bytearray(open('tablet/Image', 'rb').read())
target = 0x400000
if len(d) < target:
    d += bytes(target - len(d))
struct.pack_into('<Q', d, 0x08, 0xF3800000)   # their text_offset
struct.pack_into('<Q', d, 0x10, 0x400000)     # their image_size
open('tablet/Image', 'wb').write(bytes(d))
print('padded to', hex(len(d)))
PYINNER

echo "==> Verifying ARM64 magic at 0x38 (must be ARMd):"
xxd -s 0x38 -l 8 tablet/Image

echo "==> Gzipping Image (original kernel was Image.gz)"
gzip -9n -c tablet/Image > tablet/Image.gz  # -n: no FNAME/MTIME, ABL decompressors are strict
ls -l tablet/Image.gz

echo "==> (optional) Appending the live DTB: Image.gz-dtb"
cat tablet/Image.gz tablet/tablet.dtb > tablet/Image.gz-dtb 2>/dev/null ||
    cat tablet/Image.gz "$(dirname "$0")/tablet.dtb" > tablet/Image.gz-dtb
ls -l tablet/Image.gz-dtb

pack() {
    mkbootimg \
        --kernel "$1" \
        --header_version "$HEADER_VERSION" \
        --cmdline "console=ttyMSM0,115200n8 earlycon" \
        --os_version 13.0.0 --os_patch_level 2023-04 \
        "${EXTRA_MKBOOTIMG[@]}" \
        -o "$2"
}

echo "==> Packing boot_silver.img (header v$HEADER_VERSION)"
pack tablet/Image.gz tablet/boot_silver.img

echo "==> Packing boot_silver_dtb.img (kernel + DTB)"
pack tablet/Image.gz-dtb tablet/boot_silver_dtb.img

ls -l tablet/boot_silver.img tablet/boot_silver_dtb.img

echo "==> Header check:"
unpack_bootimg --boot_img tablet/boot_silver.img --out /tmp/silver_boot_check |
    grep -E "magic|kernel_size|header version|command line"
echo "==> Done: tablet/boot_silver.img (+ _dtb variant)"
