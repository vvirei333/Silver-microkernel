#!/usr/bin/env bash
# Fog path integration tests on QEMU `virt`.
#
# This is NOT a hardware test and does not touch a phone. It validates the
# Silver-side boot logic with a synthetic khaje-like DTB:
#
#   1. `fog-reloc` build entered 4 MiB above the link address: the explicit
#      relocation opt-in must copy the image and still reach `howl>`.
#   2. standard `fog` build entered exactly at the link address: no copy is
#      needed and the default fail-closed path must boot normally.
#   3. standard `fog` build entered 4 MiB above the link address: relocation
#      is disabled by default, so the stub must publish diagnostics and park
#      without booting.
#
# Requirements: cargo (aarch64-unknown-none target), qemu-system-aarch64,
# dtc, and llvm-objcopy or aarch64-linux-gnu-objcopy.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

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

for tool in "$CARGO" qemu-system-aarch64 dtc python3; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: missing $tool" >&2
        exit 1
    }
done

cd "$ROOT"

build() {
    local features="$1" destination="$2"
    "$CARGO" build --release --target aarch64-unknown-none \
        --no-default-features --features "$features"
    "$OBJCOPY" -O binary target/aarch64-unknown-none/release/silver_microkernel "$destination"
}

# QEMU's raw arm64 loader places the image at RAM_BASE + text_offset. Patch
# only that header field; the production image keeps text_offset = 0.
set_text_offset() {
    local source="$1" target="$2" offset="$3"
    python3 - "$source" "$target" "$offset" <<'PY'
import sys
from pathlib import Path
source = Path(sys.argv[1])
target = Path(sys.argv[2])
offset = int(sys.argv[3], 0)
data = bytearray(source.read_bytes())
data[0x08:0x10] = offset.to_bytes(8, "little")
target.write_bytes(data)
PY
}

build "no_std,fog" "$TMP/Image.std"
build "no_std,fog,fog-reloc" "$TMP/Image.reloc"

# Link address 0x4008_0000, RAM base 0x4000_0000.
set_text_offset "$TMP/Image.std" "$TMP/Image.std.at-link" 0x80000
set_text_offset "$TMP/Image.std" "$TMP/Image.std.high" 0x400000
set_text_offset "$TMP/Image.reloc" "$TMP/Image.reloc.high" 0x400000

cat > "$TMP/fog_test.dts" <<'DTS'
/dts-v1/;

/ {
    #address-cells = <2>;
    #size-cells = <2>;
    model = "Synthetic khaje test tree (QEMU virt)";
    compatible = "qcom,khaje-idp", "qcom,sm6225";

    memory@40000000 {
        device_type = "memory";
        reg = <0x0 0x40000000 0x0 0x08000000>;
    };

    reserved-memory {
        #address-cells = <2>;
        #size-cells = <2>;
        ranges;
        probe_reserved: probe@47f00000 {
            reg = <0x0 0x47f00000 0x0 0x00100000>;
            no-map;
        };
    };

    chosen {
        bootargs = "console=ttyAMA0,115200";
        stdout-path = "/soc/pl011@9000000:115200";
    };

    soc {
        #address-cells = <2>;
        #size-cells = <2>;
        ranges;

        intc: interrupt-controller@8000000 {
            compatible = "arm,gic-v3";
            reg = <0x0 0x08000000 0x0 0x00010000>,
                  <0x0 0x080a0000 0x0 0x00140000>;
            redistributor-stride = <0x00020000>;
            #interrupt-cells = <3>;
            interrupt-controller;
        };

        timer {
            compatible = "arm,armv8-timer";
            interrupts = <1 13 4>, <1 14 4>, <1 11 4>, <1 10 4>;
            interrupt-parent = <&intc>;
        };

        pl011@9000000 {
            compatible = "arm,pl011";
            reg = <0x0 0x09000000 0x0 0x00001000>;
            interrupts = <0 1 4>;
            interrupt-parent = <&intc>;
            status = "okay";
        };
    };
};
DTS

dtc -I dts -O dtb -o "$TMP/fog_test.dtb" "$TMP/fog_test.dts" 2>/dev/null

run_qemu() {
    local image="$1" log="$2" seconds="$3"
    set +e
    timeout "$seconds" qemu-system-aarch64 \
        -machine virt,gic-version=3 \
        -cpu cortex-a72 -smp 2 -m 128M \
        -kernel "$image" \
        -dtb "$TMP/fog_test.dtb" \
        -display none -serial stdio -monitor none -no-reboot \
        >"$log" 2>&1
    set -e
}

require_boot() {
    local log="$1" label="$2"
    local missing=0 marker
    for marker in \
        "SILVER FOG ENTRY" \
        "SILVER BOOT PARAMS OK" \
        "SILVER FDT OK" \
        "SILVER MEMORY MAP OK" \
        "SILVER UART OK" \
        "SILVER MMU OK" \
        "SILVER FRAME ALLOCATOR OK" \
        "SILVER HEAP OK" \
        "SILVER PLATFORM READY" \
        "howl>"; do
        if ! grep -aqF "$marker" "$log"; then
            echo "MISSING ($label): $marker"
            missing=1
        fi
    done
    if [ "$missing" -ne 0 ]; then
        echo "--- $label log tail ---"
        tail -40 "$log"
        return 1
    fi
}

fail=0

echo "== fog-reloc: entered at 0x40400000, linked at 0x40080000 =="
run_qemu "$TMP/Image.reloc.high" "$TMP/reloc.log" 40
require_boot "$TMP/reloc.log" "fog-reloc" || fail=1
grep -aE "SILVER BOOT PARAMS|SILVER MEMORY MAP|SILVER PLATFORM READY|GICV3:|TIMER TEST|howl>" \
    "$TMP/reloc.log" | head -10 || true

echo "== fog default: entered at the link address 0x40080000 =="
run_qemu "$TMP/Image.std.at-link" "$TMP/atlink.log" 40
require_boot "$TMP/atlink.log" "fog-at-link" || fail=1
grep -aE "SILVER BOOT PARAMS|SILVER PLATFORM READY|GICV3:|TIMER TEST|howl>" \
    "$TMP/atlink.log" | head -6 || true

echo "== fog default: entered at 0x40400000 with relocation disabled =="
run_qemu "$TMP/Image.std.high" "$TMP/park.log" 5
if grep -aqF "howl>" "$TMP/park.log"; then
    echo "FAIL (fail-closed): standard fog build reached howl> from a non-link address"
    fail=1
elif grep -aqF "SILVER BOOT PARAMS OK" "$TMP/park.log"; then
    echo "FAIL (fail-closed): standard fog build continued after a non-link entry"
    fail=1
else
    echo "PASS (fail-closed): no console output, no howl>"
fi

if [ "$fail" -ne 0 ]; then
    echo "FOG QEMU TEST: FAIL"
    exit 1
fi

echo "FOG QEMU TEST: PASS"
echo "relocation: explicit fog-reloc 0x40400000 -> 0x40080000"
echo "default fog: boots at 0x40080000; parks at 0x40400000 without howl>"
