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

### A. Довести память планшета (главное!)
1. **Расширить DRAM-маппинг до 2 ГБ** (0x80000000..0x1_0000_0000), чтобы покрыть
   frame-allocator регион (0xD5100000+). Сейчас бутстаб/mmu мапят 1 ГБ.
   - В бутстабе (main.rs global_asm): сейчас 6 таблиц TABLES (low L0, high L0, L1, MMIO L2,
     DRAM L2, PCI L2). Нужна вторая DRAM L2 (итого 7 таблиц) и 2 L1-входа (индексы 2 и 3),
     либо один 2 ГБ блок через L1-block-descriptor.
   - В `mmu.rs`: `TABLE_COUNT` 6→7, `INDEX_LEVEL2_DRAM` + вторая таблица, в `init()` два
     L1-входа `DRAM_L1_INDEX` и `DRAM_L1_INDEX+1`.
2. **frame_allocator регион**: для tablet `available = [0xD5100000, 0xD8000000)` (47 МБ,
   первый System RAM кусок после запретного региона) — ИЛИ регион 6 ГБ @ 0x980000000.
   - В `memory.rs`: добавить tablet-константы `FRAME_ALLOC_START/SIZE`, в `current()`
     под `#[cfg(feature="tablet")]` вернуть фиксированный регион (не `kernel_image.end..DRAM.end`).
   - Ослабить `memory::is_valid()`: `available.end == dram.end` → `<=` (+ `dram.contains(start)`).
3. `DRAM_SIZE` для tablet поднять до 2 ГБ (0x80000000), `DRAM_END=0x1_0000_0000`.

### B. GICv3 (нужно для таймера и шедулера)
- Текущий `gic.rs` — GICv2 (DISTRIBUTOR 0x08000000, CPU IF 0x08010000). Для планшета нужен
  GICv3: GICD 0x17100000, GICR 0x17180000 (per-CPU redistributor). Написать `gicv3` или
  расширить `gic.rs` через `platform::detect()`. Таймер PPI 14 (вместо 30), UART SPI 462.
- После GICv3 убрать гейт в `kernel_main` (п. tablet-halt) и запустить шедулер.

### C. PSCI smc
- В `syscall.rs` `sys_power_off()` сейчас `hvc #0`. Под tablet → `smc #0` (по `platform::detect()`).

### D. boot.img + fastboot
- Собрать raw Image: `aarch64-linux-gnu-objcopy -O binary target/.../silver_microkernel
  tablet/Image` (ядра собрано под `--features "no_std,tablet"`, LMA=0xA8000000).
- `mkbootimg` (v2, чтобы задать адрес) или v3. Проверить, что ABL грузит по 0xA8000000.
- `fastboot boot boot.img`.

### E. Юзерспейс (отдельно, позже)
- `elf.rs` держит `USER_IMAGE_VA = 0x40000000` (и `userspace/linker.ld` линкует юзер-программы
  на 0x40000000). Для планшета юзер-VA должны уйти на 0x80000000+. В `mmu.rs` я уже сделал
  `USER_IMAGE_VA = DRAM_BASE` (таблетка → 0x80000000), НО `elf.rs`/`userspace/linker.ld`
  ещё на 0x40000000 — рассогласование. На первом бутe юзер-задач нет (шедулер загейчен),
  поэтому не критично, но перед запуском юзерспейса нужно перелинковать.

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
