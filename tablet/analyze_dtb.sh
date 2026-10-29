#!/usr/bin/env bash
# Step 1 — pull & decode the live device tree from the Legion Y700 (SM8650 "pineapple").
#
# Usage: ./analyze_dtb.sh
#
# The tablet is reachable over ADB with Magisk root (`su -c`). The live flattened
# device tree is exposed at /sys/firmware/fdt and, unlike the boot image, already
# reflects the exact hardware the kernel will see after boot.
set -euo pipefail

cd "$(dirname "$0")"

# --- 0. Pull the live FDT from the device (only if we don't already have it) ---
if [ ! -s tablet.dtb ]; then
    echo "[1/3] Pulling /sys/firmware/fdt from the device..."
    adb shell "su -c 'cat /sys/firmware/fdt > /data/local/tmp/tablet.dtb && chmod 644 /data/local/tmp/tablet.dtb'"
    adb pull /data/local/tmp/tablet.dtb ./tablet.dtb
    echo "       pulled $(stat -c %s tablet.dtb) bytes"
else
    echo "[1/3] tablet.dtb already present ($(stat -c %s tablet.dtb) bytes), skipping pull"
fi

# --- 1. Decompile .dtb -> .dts -------------------------------------------------
echo "[2/3] Decompiling tablet.dtb -> tablet.dts"
dtc -I dtb -O dts -o tablet.dts tablet.dtb 2>dtc_err.log
echo "       wrote tablet.dts ($(wc -l < tablet.dts) lines); warnings in dtc_err.log"

# --- 2. Grep for the nodes the port needs -------------------------------------
echo "[3/3] Key hardware nodes"
echo
echo "== MEMORY (node 'memory', 'ddr-regions') =="
grep -nE "^\s*(memory|ddr-regions)\s*\{" tablet.dts
echo
echo "== UART (Qualcomm GENI: 'serial@' / 'qcom,qup_uart@' / stdout-path) =="
grep -niE "stdout-path|serial@|qcom,qup_uart@|qcom,geni-debug-uart" tablet.dts | head -20
echo
echo "== GIC (interrupt-controller@ / arm,gic-v3) =="
grep -niE "interrupt-controller@|arm,gic-v3" tablet.dts | head -10
echo
echo "== PSCI (psci node / enable-method) =="
grep -niE "^\s*psci\s*\{|arm,psci-1\.0|arm,psci-0\.2|method = \"smc\"|method = \"hvc\"" tablet.dts | head -10

echo
echo "Done. Inspect the exact reg/address values with:"
echo "  sed -n '15,25p' tablet.dts      # memory reg"
echo "  sed -n '860,890p' tablet.dts    # GIC intc reg"
echo "  sed -n '17810,17840p' tablet.dts # debug UART reg"
echo "  sed -n '763,770p' tablet.dts    # psci method"
