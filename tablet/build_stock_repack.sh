#!/usr/bin/env bash
# Kimi isolation experiment #2: stock kernel repack.
#
#   ./tablet/build_stock_repack.sh
#
# Takes the stock ColorOS kernel (extracted from the live boot partition —
# /tmp/boot_cur.img pulled earlier) and repacks it with OUR mkbootimg flags.
# Expected outcome: if the repacked stock image boots as usual (Android
# starts), our mkbootimg invocation/header is not what breaks the boot, and
# the failure is specific to our payload. If even this QCDUMPs, the packaging
# step itself is broken.
set -euo pipefail
cd "$(dirname "$0")/.."

STOCK_KERNEL="${1:-/tmp/cur_boot_unpack/kernel}"
if [ ! -f "$STOCK_KERNEL" ]; then
    echo "Stock kernel not found at $STOCK_KERNEL" >&2
    echo "Extract it first: unpack_bootimg --boot_img /tmp/boot_cur.img --out /tmp/cur_boot_unpack" >&2
    exit 1
fi
ls -l "$STOCK_KERNEL"

mkbootimg \
    --kernel "$STOCK_KERNEL" \
    --header_version 4 \
    --cmdline "console=ttyMSM0,115200n8 earlycon" \
    -o tablet/boot_stock_repack.img
ls -l tablet/boot_stock_repack.img

echo "==> Header check:"
unpack_bootimg --boot_img tablet/boot_stock_repack.img --out /tmp/stock_check |
    grep -E "magic|kernel_size|header version"
echo "==> Done: tablet/boot_stock_repack.img"
