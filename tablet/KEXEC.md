> **СТАТУС (2026-11-01): ПУТЬ ЗАКРЫТ** — rootfs/kexec-инфраструктура снесена,
> планшет возвращён на Android (см. HANDOFF.md). Файл — исторический справочник.

# Запуск Silver через kexec (Linux-трамплин, план с 2026-10-30)

UEFI/GRUB заброшены: в контексте EDK2+QCOMRAMP `msr sctlr_el1` с M:1→0 молча
убивает vCPU (TVM-трап, исключение не приходит даже на установленный
VBAR_EL1 — см. bisect-полосы в HANDOFF.md). Прямой ABL + сырой ARMd работает,
но грузит по неизвестному адресу и умирает в WDT/QCDUMP до чтения маркеров.
kexec — штатный механизм ядра: purgatory сам гасит MMU и прыгает в наш Image.

## Артефакт

`tablet/Image` — сырой arm64-образ (в header: code0, text_offset=0,
image_size incl. bss, ARMd @0x38). Собирается `./tablet/build_efi_img.sh`
(внутри cargo build → objcopy → копия Image.nopad → Image).

## Инструкция в Ubuntu-порте (tb321fu-linux)

1. kexec-tools:
   ```bash
   sudo apt install kexec-tools
   ```
2. Проверить поддержку в их ядре:
   ```bash
   zcat /proc/config.gz | grep -E "CONFIG_KEXEC|CONFIG_KEXEC_FILE"
   ```
   Достаточно `CONFIG_KEXEC=y` (классический kexec_load). Если нет —
   пересборка их ядра с этим флагом.
3. DTB: `/sys/firmware/fdt` (живой) или из Y700BOOT: `/mnt/y700/dtb/platform.dtb`.
   Наш DTB не парсит, но передаём оригинальный.
4. Скопировать наш Image (adb push → /mnt/... или по USB-SSD):
   ```bash
   sudo kexec -l /path/to/Image --dtb /sys/firmware/fdt --command-line=""
   sudo kexec -e
   ```
   - `--command-line=""` обязателен: kexec-tools на arm64 без него ругается.
   - kexec выберет адрес загрузки сам (по нашему заголовку), НЕ 0xA8000000 —
     поэтому первый запуск диагностический.

## Что увидеть после kexec -e

Бутстаб ядра рисует GREEN-полосу в splash (0xD5100000, 3 слота) и пишет в
ramoops-зоны (0xA2000000 console / 0xA2200000 pmsg, module-param layout):

| смещение | поле |
|---|---|
| +0x20 | M0: PC входа (реальный адрес загрузки!) |
| +0x28 | M1: CurrentEL |
| +0x30 | M2: SCTLR_EL1 на входе |
| +0x38/0x40 | M3/M4: нули после bss / после MMU on |
| +0x48 | DTB pointer (x0) |
| +0x50 | DTB totalsize |
| +0x20.. | ASCII `SILVER PC=XXXXXXXX EL=n` |

DTB копируется в физический скретч 0xA4000000 (не BSS — линковочный адрес
неверен до перелинковки).

После `kexec -e` Linux мёртв: если ядро зависло — зажать питание ~10 c
(тёплый ребут), затем загрузиться в **Android** и прочитать маркеры:
```bash
adb shell "su -c 'cat /sys/fs/pstore/console-ramoops-0 | grep -a SILVER'"
adb shell "su -c 'xxd -l 96 /sys/fs/pstore/console-ramoops-0'"
```
Если Android недоступен (мы затёрли его зоны) — читать из Linux-порта их
pstore, если он смотрит на те же адреса.

## Дальше

1. M0 → реальный адрес загрузки → правка `linker_tablet.ld` → пересборка.
2. Второй kexec: ядро доходит до M3/M4 → scheduler → GICv3/таймер на железе.
