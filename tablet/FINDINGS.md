# Legion Y700 (SM8650 "pineapple") — DTB findings

Source: live `/sys/firmware/fdt` pulled from the device (1023750 bytes),
decompiled to `tablet.dts` (38799 lines).

## SoC / platform
- model  = "pineapple"           (Snapdragon 8 Gen 3, SM8650)
- board  = OPD2404 (Lenovo Legion Y700 2025, codename "kirby"/TB321FU)
- kernel = 6.1.112 (Android 14 kernel), production build (adb root disabled)
- root   = Magisk `su` at /product/bin/su  (`su -c` -> uid 0)

## 1. Memory (node `memory`, line 19)
- `device_type = "memory"`, `ddr_device_type = <0x08>` (LPDDR5X)
- Fragmented `reg` with many reserved holes. The large usable chunks are:
  - 0x0000_0000_8000_0000  (2 GiB low DDR — where the kernel/bootloader loads)
  - 0x0000_0008_8000_0000  (region1)
  - 0x0000_0008_C000_0000  size 0xC000_0000 (3 GiB)
  - 0x0000_0009_8000_0000  size 0x1_8000_0000 (6 GiB main chunk, region2)
- `ddr-regions` (line 13) summarises: region0 = base 0x8000_0000 (2 GiB),
  region1 = base 0x8_8000_0000, region2 = base 0x9_8000_0000 (6 GiB).

## 2. UART (debug console)
- stdout-path = "/soc/qcom,qupv3_2_geni_se@8c0000/qcom,qup_uart@89c000:115200n8"
- Node `qupv3_se15_2uart: qcom,qup_uart@89c000`:
  - compatible = "qcom,geni-debug-uart"
  - reg = <0x89c000 0x4000>   -> base 0x0089_C000, 16 KiB
  - interrupts = <0 0x1CE 4>  -> GIC SPI 462, level
- parent SE: `qcom,qupv3_2_geni_se@8c0000` (base 0x8C_0000)

## 3. GIC
- `intc: interrupt-controller@17100000`, compatible "arm,gic-v3"
  - GICD (distributor)    : 0x1710_0000, 0x10000   (64 KiB)
  - GICR (redistributor)  : 0x1718_0000, 0x200000  (2 MiB), stride 0x40000
  - ITS                   : 0x1714_0000, 0x20000

## 4. PSCI
- `psci { compatible = "arm,psci-1.0"; method = "smc"; }`   (line 763)
- CPU enable-method = "psci" for all cores.
- NOTE: method = "smc"  ->  PSCI calls use `smc #0`, NOT `hvc #0`.
  (QEMU `virt` used HVC; real SM8650 firmware uses SMC.)

## cmdline (from chosen/bootargs) — for boot.img
  loglevel=6 kpti=0 ... rootwait ro init=/init ... (full string in tablet.dts)

## 5. Boot chain + EFI stub (2026-10-30, breakthrough)

### Splash framebuffer (live panel scanout)
- Node `splash_region` (label `cont_splash_region`), tablet.dts line ~399:
  `reg = <0x00 0xd5100000 0x00 0x2b00000>` → **base 0xD5100000, 43 MiB**
  (~2.6 frames of 1600x2560x4). GRUB renders its menu through it; the panel
  provably scans it out. `disp_rdump_region@d5100000` (line 757) also uses it.

### ramoops (confirmed)
- `ramoops@0xa2000000` (tablet.dts line 382): `reg = <0x00 0xa2000000 0x00
  0x400000>` (4 MiB). Module params win over DTB: mem 0xA2000000, console 2 MiB
  (0xA2000000..0xA2200000), pmsg 2 MiB (0xA2200000..0xA2400000).
- **pmsg-ramoops-0 NEVER appears** on this device (even after `echo > /dev/pmsg0`
  + reboot). console-ramoops-0 exists but is overwritten by Android's boot log
  each boot. → ramoops readback is unusable as a debug channel.

### EFI/PE stub (the working route)
- The stock ColorOS kernel is a PE32+ EFI stub ('MZ', machine 0xAA64, Subsystem
  EFI_APPLICATION, ImageBase=0, NO .reloc). ABL (UEFI) boots it via LoadImage.
- `fastboot boot` with a v4+MZ/PE image → **QCDUMP** (ABL's fastboot-boot path
  does not LoadImage; it expects gzip/arm64). Dead end.
- **Working route**: `fastboot boot /tmp/y700_boot.img` (the disguised EDK2
  UEFI) → Y700BOOT (FAT32, /dev/block/sda14) → QCOMRAMP (GRUB fork) → linux.cfg
  → `chainloader /Image.efi` + `boot` → StartImage → our `efi_entry`.
- The hand-rolled `build_efi_wrapper.py` was BROKEN: optional-header fields
  shifted 10 bytes (`HHH` vs `HHHH`) → Subsystem=UNKNOWN → LoadImage rejects.
  `lld-link /subsystem:efi_application /entry:efi_entry /machine:arm64 /base:0`
  generates a correct PE.
- GOP via `LocateProtocol` may be ABSENT in the port's EDK2 — paint 0xD5100000
  directly (hardcoded) instead of querying GOP.

### Milestone status (colored bands in splash)
- green (entry) + white (kernel copied to 0xA8000000) appear; NO magenta →
  the stub hangs at its own MMU-disable (code at LoadImage's arbitrary VA is not
  identity-mapped → post-MMU-off fetch aborts).
