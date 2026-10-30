#!/usr/bin/env bash
# Pulls ABL/minidump evidence from fastboot after a QCDUMP.
#
#   ./tablet/get_minidump.sh
#
# Run while the tablet sits in fastboot/bootloader. Tries, in order:
#   1. `fastboot oem minidump` + `fastboot get_staged`  (staged dump download)
#   2. `fastboot oem get_log`                           (ABL console log)
# Output: tablet/minidump.bin and/or tablet/abl_log.txt.
set -u
cd "$(dirname "$0")/.."

if ! fastboot devices | grep -q .; then
    echo "No fastboot device. Put the tablet into bootloader mode first."
    exit 1
fi

echo "==> fastboot oem minidump"
fastboot oem minidump 2>&1 | tee tablet/oem_minidump.txt || true

echo "==> fastboot get_staged tablet/minidump.bin"
if fastboot get_staged tablet/minidump.bin 2>&1 | tee tablet/get_staged.txt; then
    ls -l tablet/minidump.bin 2>/dev/null || true
fi

echo "==> fastboot oem get_log"
fastboot oem get_log 2>&1 | tee tablet/abl_log.txt || true

echo "==> Extra: dump the fastboot variables that mention load/addresses"
fastboot getvar all 2>&1 | grep -iE "kernel|load|dump|log|offset" | tee tablet/fastboot_vars.txt || true

echo "==> Done. Inspect tablet/abl_log.txt and tablet/minidump.bin (if any)."
