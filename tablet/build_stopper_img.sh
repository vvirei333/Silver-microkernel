#!/usr/bin/env bash
# Kimi isolation experiment #1: the stopper image.
#
#   ./tablet/build_stopper_img.sh
#
# Builds tablet/boot_stopper.img: arm64 header + wfe loop (~500 bytes).
# Expected outcomes after `fastboot boot tablet/boot_stopper.img`:
#   * black screen, tablet frozen (no QCDUMP)  -> ABL executes our payloads;
#     the real kernel crash is ours to fix (bisect inside the kernel).
#   * QCDUMP within seconds                    -> ABL rejects the image itself
#     (header/format/load path) — the kernel is not even reached.
set -euo pipefail
cd "$(dirname "$0")/.."

aarch64-linux-gnu-as -o /tmp/test_stopper.o tablet/test_stopper.S
aarch64-linux-gnu-ld -e _head -Ttext=0xA8000000 -o /tmp/test_stopper.elf /tmp/test_stopper.o
aarch64-linux-gnu-objcopy -O binary /tmp/test_stopper.elf tablet/test_stopper.bin
ls -l tablet/test_stopper.bin

echo "==> Magic at 0x38 (must be ARMd):"
xxd -s 0x38 -l 8 tablet/test_stopper.bin

mkbootimg \
    --kernel tablet/test_stopper.bin \
    --header_version 4 \
    --cmdline "console=ttyMSM0,115200n8 earlycon" \
    -o tablet/boot_stopper.img
ls -l tablet/boot_stopper.img

echo "==> Header check:"
unpack_bootimg --boot_img tablet/boot_stopper.img --out /tmp/stopper_check |
    grep -E "magic|kernel_size|header version"
echo "==> Done: tablet/boot_stopper.img"
