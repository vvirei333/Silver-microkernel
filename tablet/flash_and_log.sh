#!/usr/bin/env bash
# Boots tablet/boot_silver.img on the tablet via fastboot, then reads the
# kernel's RAM log back from Android.
#
#   ./tablet/flash_and_log.sh
#
# The kernel mirrors every boot milestone into Android's pstore ramoops pmsg
# zone at 0xA2200000 (src/ram_log.rs; ramoops: mem_address=0xA2000000,
# ecc=0, pmsg_size=2MiB). After the test, force-reboot the tablet into
# Android (Power ~10 s; a kernel crash shows the Qualcomm crashdump screen
# and reboots into Android by itself), then the second half reads
# /sys/fs/pstore/pmsg-ramoops-0 — no /dev/mem needed.
set -u
cd "$(dirname "$0")"

BOOT_IMG=boot_silver.img

echo "==> Waiting for fastboot device (enter fastboot/bootloader mode)"
for attempt in $(seq 1 60); do
    if fastboot devices | grep -q .; then
        echo "==> fastboot device found:"
        fastboot devices
        break
    fi
    sleep 1
    if [ "$attempt" = 60 ]; then
        echo "ERROR: no fastboot device after 60 s"
        exit 1
    fi
done

echo "==> fastboot boot $BOOT_IMG (temporary boot, nothing is flashed)"
fastboot boot "$BOOT_IMG"
echo "==> Booted, waiting 10 s"
sleep 10
echo "Планшет загрузился (или завис/crashdump). Перезагрузи в Android (Power ~10 с) и нажми Enter для чтения лога."
read -r

echo "==> Waiting for adb"
adb wait-for-device
adb devices

echo "==> Reading Silver RAM log from pstore (pmsg-ramoops-0)"
adb shell "su -c 'cat /sys/fs/pstore/pmsg-ramoops-0 2>/dev/null'" |
    tee tablet/ram_log.txt

echo ""
echo "==> SILVER lines:"
grep -a "SILVER\|EXC kind" tablet/ram_log.txt | head -40 || echo "(none found)"

echo ""
echo "==> If pmsg is empty/absent: check that the zone exists at all:"
adb shell "su -c 'cat /sys/module/ramoops/parameters/mem_address /sys/module/ramoops/parameters/ecc /sys/module/ramoops/parameters/pmsg_size'"
echo "==> (mem_address must be 0xA2000000 = 2717908992; pmsg zone starts at +2MiB = 0xA2200000)"
