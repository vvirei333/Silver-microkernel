# HANDOFF — Портирование Silver microkernel на Lenovo Legion Y700 (SM8650)

> **Статус**: QEMU работает, детект платформы готов, планшетная загрузка — наполовину.
> Этот файл — точка входа для продолжения работы другим AI. Читать целиком.

---

## 1. Цель

Загрузить ядро Silver microkernel (AArch64, Rust, no_std) на планшет Lenovo Legion Y700
(Snapdragon 8 Gen 3 / SM8650, codename "pineapple") через `fastboot boot`, в оперативку.
Ядро должно **само определять** (runtime) — оно в QEMU или на реальном железе.

**Железо**: OPD2404 (Legion Y700 2025 / "kirby" / TB321FU). ADB работает, root через Magisk
(`su -c`, `/product/bin/su`). Сейчас стоит ColorOS/Android (kernel 6.1.112).

---

## 2. КЛЮЧЕВЫЕ АДРЕСА И ФАКТЫ (проверено, не менять без причины)

### Платформа
- SoC: SM8650 "pineapple", ядра Kryo (Cortex-X4 / A720 / A520).
- MIDR_EL1: QEMU `-cpu cortex-a72` = part `0xD08`; планшет = part `0xD80..0xD82`.

### Адрес загрузки ядра (compile-time, в линкере)
- **0xA8000000** — ABL грузит ядро сюда (из `/proc/iomem`: `a8010000-a9b9ffff : Kernel code`).
- QEMU грузит по LMA ELF (0x40080000).

### UART (консоль отладки)
- **GENI SE UART @ 0x89C000**, baud 115200 8n1, GIC SPI 462 (`<0 0x1CE 4>`).
- DTB node: `qupv3_se15_2uart: qcom,qup_uart@89c000`, compatible `qcom,geni-debug-uart`.
- `stdout-path = "/soc/qcom,qupv3_2_geni_se@8c0000/qcom,qup_uart@89c000:115200n8"`.

### GIC
- **GICv3**: GICD @ 0x17100000 (64K), GICR @ 0x17180000 (2M, stride 0x40000), ITS @ 0x17140000.
- (QEMU использует GICv2 @ 0x08000000 — текущий `gic.rs` написан под GICv2.)

### Таймер
- ARM arch timer: PPI 14 (non-secure physical), частота 19.2 МГц (`clock-frequency = <0x124f800>`).
- (QEMU использует TIMER_IRQ=30.)

### PSCI
- **method = "smc"** → `smc #0`, НЕ `hvc #0` (в QEMU — HVC). Критично для power-off/reboot/вторичных ядер.

### Память (12 ГБ LPDDR5X, фрагментирована)
`/proc/iomem` (System RAM):
```
816e0000-819fffff, 81cf5000-81cfffff, 81f20000-8249ffff,
82800000-847fffff, 84a00000-86afffff,
a2400000-d4e22fff : System RAM     ← ядро (a8010000-a9b9ffff : Kernel code)
d5100000-d7ffffff : System RAM     ← первый чистый кусок ПОСЛЕ ядра (47 МБ)
e1bb0000-ff7fffff : System RAM     ← 477 МБ
880000000-8af8fffff, 8b0000000-8ba2fffff, 8c0000000-affffefff : System RAM (высокие)
```
`ddr-regions` из DTB:
- region0 = base `0x80000000`, 2 ГБ
- region1 = base `0x880000000` (34 ГБ), 4 ГБ
- region2 = base `0x980000000` (38 ГБ), **6 ГБ**

### boot.img (оригинал, 03-kubuntu-release/boot.img)
- header **v3** (нет поля kernel_addr), kernel = **Image.gz** (gzip) на offset 0x1000.
- DTB в boot.img НЕТ. Живой FDT выгружен с устройства: `tablet/tablet.dtb` (1023750 байт),
  декомпилирован в `tablet/tablet.dts` (38799 строк). См. `tablet/FINDINGS.md`.

---

## 3. КРИТИЧЕСКИЕ ТРЕБОВАНИЯ (от пользователя, соблюдать строго)

1. **Адрес загрузки 0xA8000000 — compile-time**, пишется в linker-скрипте.
2. **frame_allocator НЕ должен трогать 0xA2400000–0xD4E23000** (там ядро + структуры ABL).
   Начинать выделение с **0xD4E23000** (или с 0xD5100000 — первый System RAM кусок после
   запретного региона), **или использовать регион 6 ГБ на 0x980000000** из DTB.
3. **PSCI: `smc #0`, а не `hvc #0`** (как в QEMU).
4. **MMU**: ABL передаёт управление с ВЫКЛЮЧЕННЫМ MMU. В boot.S делать identity mapping
   (VA=PA) для первых мегабайт, чтобы код работал до включения high-half маппинга.
   (Уже сделано: бутстаб строит low identity TTBR0 + high TTBR1, см. main.rs global_asm.)

---

## 4. ЧТО УЖЕ СДЕЛАНО (текущее состояние кода)

### Новые файлы (все в `src/`)
- **`platform.rs`** — runtime-детект по `MIDR_EL1`: `Kind::Qemu` (part 0xD08) vs `Kind::Sm8650`.
  Функция `detect()`. Проверено: в QEMU печатает `PLATFORM=QEMU`.
- **`console.rs`** — единая точка UART; маршрутизирует на `pl011` (QEMU) или `geni_uart`
  (планшет). API = `init/write_str/write_byte/write_bytes/write_hex_u64/write_dec_u64/
  read_byte/handle_irq/take_stop_request`. Платформа кэшируется в `AtomicU8`.
- **`geni_uart.rs`** — драйвер GENI SE UART (TX-only, polling). Регистры взяты из Linux
  `qcom_geni_serial.c`/`geni-se.h` (см. п. 5 ниже). Помечен `#![allow(dead_code)]`
  (ещё не активирован). Имеет тот же API, что и `pl011`.
- **`boot_metrics.rs`** — вехи загрузки через `cntpct_el0`/`cntfrq_el0`, формат
  `[ 0.123 s ] name`. Печатает через `console`. Работает в QEMU.

### Изменённые файлы
- **`Cargo.toml`** — добавлен feature `tablet = []`.
- **`build.rs`** — выбирает линкер по feature: `-Tlinker_tablet.ld` (tablet) или `-Tlinker.ld`.
- **`.cargo/config.toml`** — убран `-C link-arg=-Tlinker.ld` (перенесён в build.rs),
  оставлен `--gc-sections`.
- **`linker.ld`** — добавлен символ `__dram_base = 0x40000000;`.
- **`linker_tablet.ld`** — копия linker.ld: `. = 0xFFFF0000A8000000` (VMA, LMA=0xA8000000),
  `__dram_base = 0x80000000;`.
- **`src/main.rs`** — бутстаб использует `ldr x15, =__dram_base` и вычисляет L1-индекс DRAM
  динамически (`lsr/lsl x16`); `kernel_main` печатает `PLATFORM=...` и **гейтит
  gic/timer/scheduler на планшете** (первый бут — только MMU+память+UART, затем
  `TABLET BOOT OK — halting`).
- **`src/mmu.rs`** — `DRAM_L1_INDEX = DRAM_BASE >> 30`; user-VA стали относительными
  (`USER_IMAGE_VA = DRAM_BASE`, heap/stack/mmap выведены из него).
- **`src/memory.rs`** — `DRAM_BASE`/`DRAM_SIZE` под `#[cfg(feature="tablet")]`.
  Сейчас для tablet: `DRAM_BASE=0x80000000`, `DRAM_SIZE=0x40000000` (1 ГБ) — **это ещё надо
  доработать под 2 ГБ + запретный регион (см. п.6)**.
- **`src/scheduler.rs`** — веха `SILVERD spawn` после `spawn_user(SILVERD)`.
- `allocator.rs/elf_loader.rs/exceptions.rs/gic.rs/seed.rs/syscall.rs` — механический
  `pl011::` → `console::` (rename, семантика не менялась).

### Что проверено в QEMU
- `PLATFORM=QEMU`, MMU/HEAP/SILVERD spawn/Scheduler start, загрузка до `howl>`.
- Оба билда собираются чисто: `cargo build --release --target aarch64-unknown-none
  --no-default-features --features no_std` и `... --features "no_std,tablet"`.

---

## 5. Регистры GENI UART (для справки, уже реализованы в geni_uart.rs)

Из Linux (`geni-se.h` / `qcom_geni_serial.c`, kernel 6.1 android14):
- `SE_GENI_M_CMD0=0x600`, `M_CMD_CTRL=0x604`, `M_IRQ_STATUS=0x610`, `M_IRQ_CLEAR=0x618`
- `SE_GENI_TX_FIFON=0x700`, `TX_WATERMARK=0x80c`, `TX_FIFO_STATUS=0x800`
- `SE_UART_TX_WORD_LEN=0x268`, `TX_STOP_BIT_LEN=0x26c`, `TX_TRANS_LEN=0x270`,
  `TX_TRANS_CFG=0x25c`, `TX_PARITY_CFG=0x2a4`
- packing 8-бит/4 слова: `TX_PACKING_CFG0=0x260 = 0x4380E`, `TX_PACKING_CFG1=0x264 = 0xC5E0E`
- opcode `UART_START_TX=1 << 27`; биты `M_TX_FIFO_WATERMARK_EN=1<<30`, `M_CMD_DONE_EN=1<<0`.

Последовательность записи байта (polling): TX_TRANS_LEN=1 → M_CMD0=START_TX → ждать watermark →
писать в TX_FIFON → clear watermark → ждать done → clear done. (См. geni_uart.rs `write_byte`.)

---

## 6. ЧТО ОСТАЛОСЬ СДЕЛАТЬ (по порядку)

### A. Довести память планшета — ✅ СДЕЛАНО (2026-10-29)
1. **DRAM-маппинг 2 ГБ** — сделано: `mmu.rs` TABLE_COUNT 6→7 (под feature `tablet`),
   вторая L2 `INDEX_LEVEL2_DRAM_HIGH` мапит `0xC0000000..0x1_00000000` в `mmu::init()`
   (бутстаб не трогали — он мапит только первый гигабайт, второй достраивается уже
   через high alias). `memory.rs`: tablet `DRAM_SIZE = 0x8000_0000`.
2. **frame_allocator регион**: `memory.rs` — tablet-константы
   `FRAME_ALLOC_START = 0xD510_0000`, `FRAME_ALLOC_END = 0xD800_0000` (47 МБ, первый
   System RAM кусок после запретного `0xA2400000..0xD4E23000`); `current()` под
   `#[cfg(feature = "tablet")]` возвращает этот фиксированный регион.
   `is_valid()` ослаблен: `available.end <= dram.end && dram.contains(available.start)`.
   `MAX_PHYSICAL_PAGES` вырос до 2 ГБ/4 КБ → bitmap 64 КБ в .bss — ок.
   В `kernel_main` добавлен вывод `FRAME=<start>..<end> PAGES=<n>` (ожидается
   `FRAME=00000000D5100000..00000000D8000000 PAGES=12032` на планшете).
   QEMU-регрессии нет (`FRAME=0000000040207000..0000000048000000 PAGES=32249`, boot до `howl>`).

### B. GICv3 — ✅ СДЕЛАНО (2026-10-29), проверить на железе
- Новый **`src/gicv3.rs`**: GICD 0x17100000, GICR 0x17180000 (stride 0x40000,
  ядро однoядерное — только core-0 редистрибьютор, SGI-фрейм на +0x10000).
  Последовательность: `icc_sre_el1.SRE=1` → будим GICR (WAKER.ProcessorSleep=0,
  ждём ChildrenAsleep=0) → SGI/PPI: IGROUPR0/IGRPMODR0 (Group 1 NS) + приоритет 0xA0
  + ISENABLER0 bit14 (таймер INTID 30) → UART SPI 462 (INTID 494) настроен в GICD
  (IGROUPR/IGRPMODR/ICFGR level/приоритет 0x80), но **НЕ включён** (geni_uart пока
  polling TX-only — включённая линия дала бы необработанные IRQ) → GICD_CTLR=
  EnableGrp1NS + опрос RWP → ICC_CTLR_EL1=0, ICC_PMR_EL1=0xFF, ICC_IGRPEN1_EL1=1.
- `gic.rs` теперь диспетчер: tablet → gicv3 (init/enable/disable/acknowledge/
  end_interrupt/silver_handle_irq), иначе GICv2. scheduler.rs не менялся.
- Таймер: PPI 14 DTB = INTID 30 — совпало с QEMU (TIMER_IRQ=30); timer.rs читает
  cntfrq_el0 (19.2 МГц), `cntp_tval/ctl_el0` — NS physical timer, без изменений.
- Гейт tablet-halt в `kernel_main` снят: tablet идёт по общей загрузке (gic→timer→
  vfs→seed→scheduler). IRQ маска снимается в scheduler::init (`daifclr, #2`).
  **FIQ (`daifclr, #3`) намеренно НЕ включён**: у нас Group 1 NS → IRQ; включение
  FIQ без Group 0-настройки направило бы FIQ в дефолтный обработчик исключений.
- ВАЖНО: запуск юзерспейса на планшете упадёт/не заработает до пункта E
  (перелинковка userspace на 0x80000000+) — silverd-ELF линкуется на 0x40000000.
  Ждать на серийном выводе: PLATFORM=SM8650 → MMU=OK → FRAME=…D5100000..…D8000000
  → HEAP=OK → IRQ READY → SCHEDULER START (таймерные тики видны по uptime в fastfetch).
- QEMU-регрессии нет (boot до `howl>`); tablet-билд собирается чисто.

### C. PSCI smc — ✅ СДЕЛАНО (2026-10-29)
- Единственное место с PSCI — `src/syscall.rs::sys_power_off` (SYSTEM_OFF
  0x8400_0008, x1-x3=0 по SMC Calling Convention). Под `feature = "tablet"` —
  `smc #0`, иначе `hvc #0`. CPU_ON/вторичные ядра ядром пока не вызываются
  (однoядерный шедулер) — при добавлении использовать тот же conduit-паттерн.

### D. boot.img + fastboot — ОСТАЛОСЬ (следующий шаг)
- Собрать raw Image: `aarch64-linux-gnu-objcopy -O binary target/.../silver_microkernel
  tablet/Image` (ядро собрано под `--features "no_std,tablet"`, LMA=0xA8000000).
- `mkbootimg` (v2, чтобы задать адрес) или v3. Проверить, что ABL грузит по 0xA8000000.
- `fastboot boot boot.img`.

### E. Перелинковка userspace — ✅ СДЕЛАНО (2026-10-29)
- `userspace/linker_tablet.ld` (база `. = 0x80000000`) рядом с `linker.ld`
  (0x40000000). Выбор скрипта перенесён из `userspace/.cargo/config.toml` в новый
  `userspace/build.rs` (по `CARGO_FEATURE_TABLET`); в userspace `Cargo.toml`
  добавлена пустая фича `tablet = []`.
- Корневой `build.rs` при tablet-сборке ядра передаёт `--features tablet` в
  userspace-подсборку → silverd линкуется на 0x80000000 (проверено readelf).
- `src/elf.rs::USER_IMAGE_VA` стал cfg-константой: tablet → 0x8000_0000
  (совпадает с `mmu::USER_IMAGE_VA = DRAM_BASE`), QEMU → 0x4000_0000. Валидация
  сегментов теперь автоматически принимает 0x80000000.
- ВАЖНО: userspace-ELF-ы для планшета получаются ТОЛЬКО из tablet-сборки ядра
  (встраиваются в бинарник kernel'а на этапе build.rs). Ручная сборка
  `cd userspace && cargo build` без фичи даёт 0x40000000-варианты.
- QEMU-регрессии нет: silverd на 0x40000000, boot до `howl>`; tablet-билд
  собирается, entry ядра 0xA8000000.

---

## 7. КОМАНДЫ СБОРКИ

```bash
# QEMU (default)
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std

# Планшет
cargo build --release --target aarch64-unknown-none --no-default-features --features "no_std,tablet"

# alias'ы (из .cargo/config.toml): cargo kernel-release  /  + --features "no_std,tablet"

# QEMU-запуск (диск SFS нужен sfs_server'у)
./build_and_qemu.sh release          # собирает + запускает

# Юзерспейс вручную (если ELF-загрузка падает — см. п.8)
cd userspace && cargo build --release --target aarch64-unknown-none
```

---

## 8. ГРАБЛИ (важно!)

1. **Два варианта (QEMU/tablet) пишут в ОДИН файл** `target/aarch64-unknown-none/release/
   silver_microkernel`. Tablet-билд перезаписывает QEMU-билд и наоборот. **Перед тестом
   пересобирай нужный вариант.** (Лучше: раздельные `--target-dir`.)
2. **RUSTFLAGS не должен утекать в userspace-подсборку** (критичный баг, уже исправлен):
   `build.rs` запускает `cargo build` в `userspace/`, и тот наследует
   `CARGO_ENCODED_RUSTFLAGS` ядра. Если в root `.cargo/config.toml` убрать
   `-Tlinker.ld`, userspace-бинари линкуются на дефолтный 0x200000 (а не 0x40000000),
   и ядро падает на `ELF: reject - segment out of range` при спавне silverd.
   **Фикс** (уже в build.rs): `command.env_remove("RUSTFLAGS").env_remove("CARGO_ENCODED_RUSTFLAGS")`
   перед запуском userspace-сборки. Если снова увидишь `segment out of range` —
   проверь, что silverd залинкован на 0x40000000: `llvm-readelf -l target/userspace/aarch64-unknown-none/release/silverd | grep LOAD`.
3. **QEMU PSCI = HVC, планшет = SMC** — не перепутать (см. п.3).
4. **Howler input**: команды >16 символов в одной UART-пачке теряются (mailbox 16).
   В тестах слать посимвольно (~40 мс).
5. Первый планшетный бут **намеренно останавливается** на `TABLET BOOT OK — halting`
   (gic/timer/scheduler загейчены в `kernel_main`). Убрать гейт после п. B.
6. Глобальный rename `pl011::` → `console::` уже сделан; `pl011.rs` ЖИВ (не удалять),
   `geni_uart.rs` жив. Ничего не удалять.

---

## 9. Полезные файлы в `tablet/`
- `tablet.dtb` (живой FDT с устройства), `tablet.dts` (декомпилированный)
- `FINDINGS.md` (сводка DTB-анализа), `analyze_dtb.sh` (pull→decode→grep)
- `/run/media/ivan/New Volume/Kirby-Y700-2026-09-26/03-kubuntu-release/extracted/`
  — оригинальный boot.img (распакован) + kernel.raw (Image.gz распакован в arm64 Image)

## 10. Полезные ADB-команды (планшет подключён, root через Magisk)
```bash
adb shell "su -c 'cat /proc/iomem'"          # адрес ядра / System RAM
adb shell "su -c 'cat /sys/firmware/fdt' > /data/local/tmp/fdt && chmod 644 ...'"
adb pull /data/local/tmp/fdt tablet.dtb
fastboot devices                              # (пусто, пока устройство в Android)
```

---

## 11. СЕССИЯ 2026-10-30: бут-цепочка UEFI вскрыта, payload не стартует — читать перед продолжением!

### Что сделано (все шаги 1-3 + E закрыты, D — в процессе)
1. **Память** ✅: DRAM 2 ГБ, frame allocator 0xD6200000..0xD8000000.
   **ВАЖНО: 0xD5100000..0xD60B0000 = живой framebuffer (cont_splash, 1600×2560 XRGB32,
   16.4 МБ на кадр, всего 43 МБ)** — frame allocator сдвинут выше, чтобы не рисовать
   мусор на экране. QEMU-регрессий нет.
2. **GICv3** ✅ (`src/gicv3.rs`, gic.rs = диспетчер). **PSCI smc** ✅. **userspace 0x80000000** ✅.
3. **ram_log.rs** — RAM-консоль в pstore ramoops pmsg (0xA2200000, dc cvac после записи),
   RAM-маркеры в бутстабе (K0-K3), framebuffer-полосы прогресса (`fb_progress`,
   красный экран при исключении). **На tablet console:: полностью выключен**
   (QEMU-MMIO под feature=tablet не дёргается).
4. **Мини-векторы** в бутстабе (VBAR EL1+EL2 до рискованных операций, дамп ESR/FAR/ELR в pmsg).

### Вскрытая бут-цепочка устройства (факты, проверено экспериментами!)
- **boot.img порта (Kubuntu/`vitalo233-eng/tb321fu-linux`) — это EDK2 UEFI,
  замаскированное под arm64 Image** (v3 + gzip + магия ARM\x64). ABL грузит его как ядро,
  оно поднимает UEFI → GRUB-форк **QCOMRAMP** (guf296, "grub2-ram-partition-ab") →
  `linuxdirect /Image` → Linux.
- ABL: payload **без** ARM\x64-магии на 0x38 → мгновенный QCDUMP. С магией — грузится глубже.
- **USB в UEFI не работает** (FSA4480 не подаёт VBUS до ядра) — флешки не видны, только eMMC.
- **Y700BOOT создан БЕЗ потери данных**: переименованы пустые `rawdump`(256МБ)+`pstoredump`
  (128МБ) в один Y700BOOT 384МБ (ESP GUID, имя "Y700BOOT"); `lenovoraw` восстановлен.
  Бэкапы: `tablet/backups/` (GPT стока, хвост GPT, lenovoraw, QCOMRAMP/BOOTAA64/platform.dtb).
- **QCOMRAMP требует**: FAT32 (FAT16 GRUB-форк не читает!), 4К-сектора, файлы:
  `/Image`, `/dtb/platform.dtb`, `/EFI/BOOT/{BOOTAA64.EFI=QCOMRAMP, QCOMRAMP.EFI, linux.cfg, grub.cfg}`.
- **linux.cfg ОБЯЗАН содержать `qcomfdtmem source rampartition` + `qcomfdtmem disable-reserved
  ...4 пути...` перед linuxdirect** — иначе «table info not found» → QCDUMP.
  Встроенный конфиг QCOMRAMP лежит в бинарнике (offset ~0x3d1ce9, menuentry "TB321FU Linux").
- **ARM64-заголовок Image: text_offset = 0** (GRUB использует как смещение прыжка).
- ramoops: **pmsg = 0xA2200000** (модульные параметры: mem 0xA2000000, 4МБ, console 2МБ,
  pmsg 2МБ; DTB-узел врёт про 2МБ), после краша с RAMDUMP зоны стираются; тёплый ребут — нет.

### ТЕКУЩАЯ ЗАГВОЗДКА
GRUB-цепочка доходит до `qcomfdtmem: disabled reserved-memory paths: 4`, затем
`linuxdirect /Image` → тишина: **ни одного SILVER-маркера в ramoops, ни одной полосы
K0 (рисование в 3 кадра splash прямо из первых инструкций бутстаба)**.
→ Скорее всего linuxdirect НЕ прыгает в наш payload (не-exec Image? проверка формата?
jump мимо?) или прыгает в неверное место. Эксперименты (стоппер-wfe, K0-краска)
неоднозначны: стоппер «висел», но это может быть зависание самого GRUB.

### СЛЕДУЮЩИЕ ШАГИ (по приоритету)
1. **kexec через их Linux (рекомендовано юзером)**: поставить их Ubuntu-порт по
   официальному гайду (раздел 10 этого файла, НУЖЕН factory reset userdata! или
   USB-SSD с rootfs по их docs) → из рабочего Linux: `kexec -l Image --dtb
   platform.dtb && kexec -e` — гарантированно чистый handover, мимо ABL/GRUB.
2. Альтернатива: **PE/EFI-стаб** — заготовка `tablet/build_efi_wrapper.py`
   (PE32+ + EFI-вход печатает SILVER через ConOut). Не проверена — мой минимальный
   PE падал в QCDUMP, вероятно кривой заголовок; можно взять за основу GNU EFI или
   вытащить PE-обёртку из их Image (он MZ+PE! распакуй /tmp/orig_boot/kernel gzip
   → там полный EFI-стаб Linux — можно скопировать layout).
3. Крайний вариант диагностики: UART. console=ttyMSM0; поискать выводы UART на плате.
4. Если kexec сработает: маркеры покажут реальный адрес загрузки → при необходимости
   перелинковка (linker_tablet.ld) → шелл на экране через fb_progress.

### Команды быстрого повтора
```bash
# сборка tablet + образ для Y700BOOT
cargo build --release --target aarch64-unknown-none --no-default-features --features "no_std,tablet"
aarch64-linux-gnu-objcopy -O binary target/.../silver_microkernel /tmp/usb_Image
adb push /tmp/usb_Image /data/local/tmp/Image
adb shell "su -c 'mount -t vfat /dev/block/by-name/Y700BOOT /mnt/y700 && cp /data/local/tmp/Image /mnt/y700/Image && sync && umount /mnt/y700'"
# бут UEFI (UEFI = tablet/backups/uefi_boot_img.img)
fastboot boot tablet/backups/uefi_boot_img.img
# чтение лога после форс-ребута в Android
adb shell "su -c 'cat /sys/fs/pstore/pmsg-ramoops-0'" | grep -a SILVER
```

---

## 12. СЕССИЯ 2026-10-30 (вечер): EFI/PE-стаб ЗАРАБОТАЛ — chainloader-прорыв

### Главное
- **Наш код впервые реально выполнился на планшете.** Зелёная полоса на экране =
  стаб `efi_entry` отработал (закрасил splash-region).
- Рабочий путь — **Route 2: chainloader через GRUB** (НЕ `fastboot boot`).
- `fastboot boot` c v4+MZ/PE → мгновенный QCDUMP (тупик, как и прямой boot).

### Рабочая цепочка
`fastboot boot /tmp/y700_boot.img` (замаскированный EDK2) → Y700BOOT (FAT32,
/dev/block/sda14) → QCOMRAMP → configfile linux.cfg → меню → `chainloader
/Image.efi` + `boot` → StartImage → наш `efi_entry` → зелёная полоса.

### Файлы (новые)
- `tablet/efi_stub.S` — PE32+ EFI-стаб (`efi_entry`): красит splash, ConOut
  "SILVER BOOT", копирует ядро на 0xA8000000, выключает MMU+I/D-cache, пишет
  маркер в pmsg 0xA2200000 (с MMU off — физическая запись), `br 0xA8000000`.
- `tablet/build_efi_img.sh` — сборка: clang (COFF) + lld-link `/base:0` →
  `tablet/Image.efi` (корректный PE, не ручной).
- `tablet/Image.nopad` — сырое ядро без паддинга, встраивается `.incbin`.

### Ключевые факты/находки
- **Старый `build_efi_wrapper.py` был сломан**: optional-header поля съехали на
  10 байт (`HHH` вместо `HHHH`) → Subsystem=UNKNOWN → LoadImage отвергал → QCDUMP.
  lld-link генерит корректный PE (Subsystem=EFI_APPLICATION 0xA, ImageBase=0).
- **splash-region = 0xD5100000, 43 МБ** (tablet.dts: `reg=<0 0xd5100000 0 0x2b00000>`).
  Панель сканирует его; GRUB рисует меню через него.
- **GOP через LocateProtocol может отсутствовать** у «замаскированного» EDK2 —
  не полагаться; красить 0xD5100000 напрямую (жёстко).
- **pmsg-ramoops-0 НЕ появляется** на этом устройстве (даже после `echo > /dev/pmsg0`).
  console-ramoops-0 затирается Android при каждой загрузке. → pmsg ненадёжен,
  фреймбуфер — основной канал диагностики.
- `ram_log.rs` теперь читает fb-инфо из 0xA7FFE000 (стаб пишет {magic "SILVERFB",
  base, w, h, stride, format}), фолбэк 0xD5100000.

### Что осталось
- Ядро ещё НЕ запускается (после зелёной полосы нет цветных полос прогресса).
- Следующий шаг: цветные вехи в стабе (зелёный=вход, белый=после копирования,
  пурпурный=после MMU-off) — локализовать, где стаб встаёт.
- Кандидаты: копирование в 0xA8000000 (может затирать данные UEFI), MMU-off, прыжок.

### Прогресс 2 (цветные вехи, 2026-10-30): стаб доходит до выключения MMU
- Стаб с вехами (3 полосы в splash сверху вниз) дал: **зелёная (вход) + белая
  (ядро скопировано в 0xA8000000)**, **фиолетовой НЕТ**.
- Вывод: стаб отработал вход, ConOut, копирование ядра, чистку кэша — и
  **падает/зависает на выключении MMU** (или на маркере сразу после).
- Диагноз (классика): после `msr sctlr_el2/el1` (M=0) CPU начинает читать
  физические адреса; код стаба загружен LoadImage'ом по произвольному VA, который
  НЕ identity-mapped → следующий fetch уходит не туда → abort/зависание.
- **Важно**: pmsg-канал МЁРТВ (pmsg-ramoops-0 не появляется ВООБЩЕ, даже после
  `echo > /dev/pmsg0`; console-ramoops-0 затирается Android). Диагностика — ТОЛЬКО
  через GOP-framebuffer (0xD5100000, работает: зелёная/белая полосы видны).

### Следующий шаг (чиним MMU-off)
1. Переписать MMU-off в `efi_stub.S`: только PC-relative инструкции вокруг
   `msr sctlr`, после каждого `msr` — `isb` + `b` (переход на гарантированно
   замапленный физ. адрес). Проверить EL (EL2 → sctlr_el2, EL1 → sctlr_el1).
2. Вехи: пурпурная=EL2, голубая=EL1, жёлтая=«MMU выключен». Прыжок ТОЛЬКО после
   жёлтой.
3. Если MMU-off всё равно зависает (жёлтой нет) — **НЕ выключать MMU в стабе**,
   прыгать в ядро сразу (MMU on, identity на 0xA8000000): ядро `_start` само
   выключает MMU (K2), и его код уже на identity-адресе 0xA8000000.

### Ключевые цвета вех (BGRA, Qualcomm)
- зелёный 0xFF00FF00 · белый 0xFFFFFFFF · пурпурный 0xFFFF00FF ·
  голубой 0xFF00FFFF · жёлтый 0xFFFFFF00
