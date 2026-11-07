# Аудит дельты `84db729..87eac98`: план исправлений

Документ — задание для исполнителя (DeepSeek) по результатам аудита нового
пласта: 33 коммита, ~+7.1k строк, 6 новых модулей (kernel-SFS, virtio-blk,
приоритеты/FIFO/watchdog, bounded memory + partial munmap, GPU-композитор,
SilverHome + встроенные приложения).

Проверено на момент аудита: `cargo test --lib` — 182/182, QEMU `-smp 4`
selftest + диск — все PASS, загрузка до `howl>`.

Нумерация строк — по состоянию `87eac98`. При переносе кода ориентируйся на
имена функций, а не на номера.

## Статус выполнения

| Пункт | Состояние | Коммит / доказательство |
|---|---|---|
| SFS-1, SFS-3 | исправлено | `f2a6344`; битый root-блок → `touch: failed`, паники нет |
| SFS-2 | исправлено | `f2a6344`; 13 append'ов → 2 блока файла, `sfs_fsck` чист |
| SFS-4 | исправлено | `f2a6344`; битый `bitmap_block` → `KSFS: bad superblock`, FS не монтируется |
| SFS-5 | исправлено | `f2a6344`; порядок bitmap → данные → inode |
| SFS-6 | исправлено | `15cab04`; `VFS_LOCK` вокруг всего вызова, `-smp 4` + запись в `/sfs` |
| SFS-7 | исправлено | `15cab04`; таймаут → `STALLED`, драйвер fail-stop |
| SIL-1 | исправлено | `a61f479`; экран у первого draw-клиента |
| SIL-2 | исправлено | `a61f479`; `OP_POINTER` только от `input_driver` (QMP-тап дошёл до Calc) |
| SIL-3 | исправлено | `a61f479`; неудачная доставка освобождает владельца |
| MMU-1 | исправлено | `689a20e`; L2/L3 чистятся до линковки |
| MMU-2, MMU-3 | исправлено | `689a20e`; flush до free, откат частичного map |
| W-1 | исправлено | `689a20e`; watchdog читает atomic `current_task_of` |
| CACHE-1 | исправлено | `689a20e`; `way_shift` по `ceil(log2(ways))` |
| I-1, I-2 | исправлено | `689a20e`; BAR4 программируется и проверяется, queue size валидируется |
| I-3, I-4, I-5 | исправлено | `f2a6344`, `15cab04` |
| SCHED-1 | исправлено | `81c3cb8` targeted SGI по MPIDR после push-миграции |
| SCHED-2 | отложено | fallback-скан не отличает idle-задачу чужого CPU; изменения требуют нового поля владельца, риск выше пользы (Low) |

Проверка после пакета: `cargo test --lib` 182/182, `-smp 4` + `smp-selftest`
(remap / fifo / priority / tlb / watchdog / cache / bkl / quiesce / ticks /
clock) — PASS, `KSFS: mounted, blocks=2048`, запись и append в `/sfs`
проходят, `scripts/sfs_fsck.py` чист на всех тестовых образах.

---

## Правила для исполнителя

1. **Один пункт — один коммит.** Сообщение: `sfs: ...`, `vfs: ...`,
   `gpu: ...`, `mmu: ...`.
2. Никаких изменений ABI syscall'ов, формата SFS на диске и раскладки
   композиторских опкодов — только валидация и порядок операций.
3. После каждого коммита:
   - `./build_and_qemu.sh --no-run` — сборка без ошибок;
   - `cargo test --lib` — не меньше 182/182;
   - QEMU с диском, `-smp 1` и `-smp 4` — до `howl>` без паник;
   - для SFS — прогон записи/чтения (см. «Тест-план»).
4. Если фикс требует нового поведения (reset устройства, реестр клиентов) —
   сначала fail-stop вариант, рефакторинг потом.
5. Не трогать зелёные инварианты: BKL-окна `schedule()`/`quiesce`,
   zero-before-publication на data-путях, W^X, all-or-nothing partial munmap.

Порядок: **P0** (SFS-1/2/3) → **P1** (SFS-6, SIL-1/2) → **P2** (SFS-4/5/7,
MMU-1) → **P3** (Low).

---

## P0 — kernel panic и утечка диска

### SFS-1. `add_entry` индексирует image номерами блоков с диска без проверки

**Где:** `src/sfs.rs:269-300` (`add_entry`), вызов из `create_inode`
(`sfs.rs:334`).

**Суть:** `inode_get` возвращает `blocks` прямо из inode на диске. В
`add_entry` цикл делает `let start = block as usize * BLOCK_SIZE;` и дальше
`u32_at(image, entry)` / `image[entry + 4]`. `u32_at` и `put_u32` индексируют
срез без проверок (`sfs.rs:75-86`). Соседние `find_entry` и `read_dir`
проверяют `start + BLOCK_SIZE > image.len()` — здесь проверки нет.

**Последствие:** битый/враждебный `disk.img` (или inode, испорченный
частичной записью) даёт **panic в ядре** на первом же `CREATE`/`touch` через
`SYS_FS_*`. Это DoS ядра, а не отказ одной операции.

**Фикс:** один helper и использовать его во всех местах, где номер блока из
inode превращается в смещение:

```rust
/// Validates a data-block number read from an inode before it is used as an
/// image offset. Metadata blocks and out-of-range numbers are rejected.
fn block_start(image: &[u8], superblock: &Superblock, block: u32) -> Option<usize> {
    if block < superblock.data_block_start || block >= superblock.total_blocks {
        return None;
    }
    let start = block as usize * BLOCK_SIZE;
    if start + BLOCK_SIZE > image.len() {
        return None;
    }
    Some(start)
}
```

Применить в: `add_entry` (обе ветки — обход существующих блоков и обход
`blocks.iter_mut()`), `find_entry`, `read_dir`, `read_at`, append read-back,
`store` (запись данных), `truncate`/`free_blocks` (через SFS-итем I-3).

В `add_entry` при провале валидации — `return None` (операция отклоняется),
ничего не освобождая: вызывающий `create_inode` сам откатит inode.

**Приёмка:** `fuzz`-подобный тест на host: испортить поле `blocks[0]` в
inode (значения `0`, `data_block_start-1`, `total_blocks`,
`total_blocks+1000`, `u32::MAX`) и вызвать `create_file`/`write`/`read` —
ошибка `Err`, паники нет.

---

### SFS-2. Append утекает все старые блоки файла

**Где:** `src/sfs.rs:658-681` (append-ветка `write_at`) → `store()`
(`sfs.rs:364`).

**Суть:** append читает старое содержимое в `existing`, дописывает `bytes` и
зовёт `store(image, sb, index, &existing)`. `store` **аллоцирует новый набор
блоков** и перезаписывает inode. Старые блоки файла не освобождаются — их
биты в bitmap остаются установленными навсегда.

**Последствие:** каждое дописывание файла теряет до `min(len, 2048)` байт
диска. Notes/лог-приложение с append-семантикой съедает 2030 блоков за
~1-2 тысячи операций, после чего `alloc_block` возвращает `None` и FS
становится read-only по факту. Восстановление — только переформатированием.

**Фикс:** освобождать старые блоки **после** успешной публикации нового
inode. Переписать `store` так, чтобы она принимала старый набор:

```rust
fn store_replacing(
    image: &mut [u8],
    superblock: &Superblock,
    index: u32,
    bytes: &[u8],
    old_blocks: &[u32; DIRECT_BLOCKS],
) -> bool
```

Порядок: выделить новые → записать секторы → `inode_put` с новыми блоками
(он флашит inode-блок) → `free_blocks(old)` в памяти → `flush_bitmap`.
Крэш между шагами даёт утечку старых блоков (безопасно), но не кросс-линк.

`free_blocks` для перезаписи не должен трогать блоки, которые уже входят в
новый набор (на 4 direct-блоках пересечения быть не может, но проверку
добавить).

**Приёмка:** на host — цикл из 100 append'ов по 100 байт на файл; число
свободных блоков (`bitmap`) до и после отличается не больше чем на
`blocks_needed(итоговый размер)`, а не на 100 × 1.

---

### SFS-3. Append read-back без bounds-проверки

**Где:** `src/sfs.rs:669-672`.

**Суть:** `existing.extend_from_slice(&image[start + within..start + within + take]);`
— `start` получен из inode без проверки. Битый inode → panic.

**Фикс:** тот же `block_start()` из SFS-1; при `None` — `return Err(())`.
Дополнительно проверять, что `within + take <= BLOCK_SIZE`.

**Приёмка:** как SFS-1, отдельный тест на append к испорченному inode.

---

## P1 — SMP-корректность и модель угроз EL0

### SFS-6. `with_vfs` не сериализует между CPU

**Где:** `src/vfs.rs:1051-1056`; общий буфер образа —
`src/sfs.rs:38-70`; общая страница запроса virtio —
`src/virtio_blk.rs:239-333`.

**Суть:** `with_vfs` делает только `save_and_disable_irq()` — это запрет
прерываний на **своём** PE, не межъядерная блокировка. На `-smp 2+` два
одновременных FS-syscall'а:
- пишут в один и тот же `image` (`&'static mut [u8]` через
  `UnsafeCell`, без синхронизации) → гонка на bitmap/inode;
- используют **одну** страницу запроса, один набор дескрипторов и одно
  available/used-кольцо virtio → `used.idx`/`last_used` расходятся, DMA
  уходит в чужой буфер.

На `-smp 1` не воспроизводится — поэтому selftest зелёный.

**Фикс:** dedicated spinlock вокруг всего VFS-вызова:

```rust
static VFS_LOCK: smp::SpinLock<()> = smp::SpinLock::new(());

fn with_vfs<T>(call: impl FnOnce(&mut Vfs) -> T) -> T {
    let _guard = VFS_LOCK.lock(); // сам отключает IRQ и восстанавливает DAIF
    call(vfs_slot().expect("VFS is not initialized"))
}
```

Требования:
- `SpinLock::lock()` уже сохраняет/восстанавливает DAIF — убрать ручной
  `save_and_disable_irq`/`restore_irq`, иначе двойное восстановление.
- Проверить, что нет реентерабельности `with_vfs` (вложенный вызов =
  самодедлок). Если есть — разделить на `with_vfs_locked` для внутренних
  путей.
- Порядок захвата: **BKL → VFS_LOCK**, обратного порядка не вводить. В
  обработчике прерываний VFS не используется (проверить grep'ом).

**Приёмка:** selftest-стресс на `-smp 4`: N тасков параллельно делают
`create/write/read/unlink` в `/sfs`; после прогона — `fsck`-подобная проверка
(обход inode: нет двух inode с одним блоком, bitmap совпадает с набором
блоков) и отсутствие паник.

---

### SIL-1. Композитор без аутентификации клиента

**Где:** `userspace/src/bin/gpu_driver.rs:219-232`, `:234-274`.

**Суть:** `handle_compositor_request` принимает `GUI_CLEAR`/`GUI_RECT`/
`GUI_TEXT`/`GUI_TEXT_SCALED` от **любого** отправителя и сразу делает
`UI_CLIENT = Some(message.sender)`. Любой EL0-таск может:
- залить экран своим цветом (дефейс);
- перехватить поток координат пальца, отправив один draw-op;
- спамить draw-операциями и не давать композитору дойти до input-сообщений
  (голодание pointer-forwarding).

**Фикс:** владелец экрана — первый draw-клиент, дальше только он:

```rust
// 0 = свободно; иначе task id владельца.
static UI_CLIENT: AtomicU64 = AtomicU64::new(0);

fn claim_ui(sender: u64) -> bool {
    match UI_CLIENT.compare_exchange(0, sender, AcqRel, Acquire) {
        Ok(_) => true,
        Err(owner) => owner == sender,
    }
}
```

- draw-op от не-владельца: игнорировать, один раз напечатать
  `GPU: draw op from non-owner <id> ignored`.
- при смерти владельца (см. SIL-3) — сброс в 0.
- `AtomicU64` убрать `static mut` (в `no_std` — `core::sync::atomic`).

**Приёмка:** тестовый EL0-таск, который шлёт `GUI_CLEAR` после SilverHome:
экран не меняется, в логе ровно одна строка об игноре.

---

### SIL-2. Тач-инъекция: `OP_POINTER` от любого отправителя

**Где:** `userspace/src/bin/gpu_driver.rs:220-229` (`input_proto::decode` без
проверки `message.sender`), `:291-301`.

**Суть:** decode не смотрит на отправителя. Любой таск может прислать
`InputMsg` с произвольными `x/y/left_clicked` → композитор перешлёт это
SilverHome как тап: запуск приложений, ввод в Notes, нажатия в калькуляторе.

**Фикс:**
- при старте `gpu_driver` резолвит `let input_id = syscall::lookup("input_driver")`
  (retry-цикл, как для остальных сервисов);
- `input_proto::decode(&message)` вызывать только при
  `message.sender == input_id`;
- всё остальное — молча отбрасывать (не логировать каждый пакет, иначе DoS
  логом).

**Приёмка:** поддельный `InputMsg` из тестового таска не двигает курсор и не
порождает тап в SilverHome; реальный ввод работает.

---

## P2 — целостность FS и ритуал MMU

### SFS-4. Поля суперблока не валидируются

**Где:** `src/sfs.rs:88-114` (`superblock`), `:258-266`
(`find_free_inode`).

**Суть:** проверяются `block_size`, `total_blocks`, `inode_count != 0`,
`data_block_start < total_blocks`. Не проверяются:
- `bitmap_block < total_blocks` и что bitmap покрывает `total_blocks` бит;
- `inode_table_block < total_blocks` и что таблица inode целиком лежит в
  образе;
- `root_inode < inode_count`.

`find_free_inode` зовёт `u32_at(image, inode_offset(...)?)` — `inode_offset`
проверяет только `inode < inode_count`, но не `offset` против `image.len()`
(это делает `inode_get`, а `find_free_inode` — нет). Битый
`inode_table_block` → panic.

**Фикс:** в `superblock()` добавить:

```rust
let bitmap_end = bitmap_block as usize * BLOCK_SIZE
    + (total_blocks as usize).div_ceil(8);
let inode_end = inode_table_block as usize * BLOCK_SIZE
    + inode_count as usize * INODE_SIZE;
if bitmap_block >= total_blocks
    || inode_table_block >= total_blocks
    || bitmap_end > image.len()
    || inode_end > image.len()
    || root_inode >= inode_count
{
    return Err(());
}
```

Плюс в `find_free_inode` использовать `inode_get`-подобную проверку границ
(или общий `inode_offset_checked(image, sb, index)`).

**Приёмка:** host-тест: каждый из пяти битых вариантов суперблока →
`superblock()` возвращает `Err`, монтирование отклоняется, паники нет.

---

### SFS-5. Порядок флаша при аллокации даёт кросс-линк после крэша

**Где:** `src/sfs.rs:382-399` (`store`), `:200-208` (`alloc_block`),
`:353-362` (`truncate` — здесь порядок правильный, брать за образец).

**Суть:** `alloc_block` ставит бит в **памяти**; `store` пишет данные,
`inode_put` флашит inode-блок, и только потом `flush_bitmap`. Крэш между
флашем inode и флашем bitmap: inode уже ссылается на блок, bitmap считает
блок свободным → следующий `alloc_block` отдаст его другому файлу →
**два inode делят один блок**, взаимное затирание данных.

**Фикс:** порядок «сначала занятость, потом ссылка»:

1. `alloc_block` (в памяти) →
2. `flush_bitmap` (блоки помечены занятыми на диске) →
3. запись данных в секторы →
4. `inode_put` (inode опубликован).

Крэш на любом шаге даёт утечку блоков, а не кросс-линк. Освобождение
(truncate/rewrite) оставить как есть: `inode_put` → `flush_bitmap`
(ссылка убрана раньше, чем блок отдан).

**Приёмка:** host-тест с эмуляцией «крэш после шага N» (обёртка над
`virtio_blk::write_sector`, считающая вызовы): после «ребута» (повторный
`superblock`+обход) нет двух inode с общим блоком; допустимы только
свободные, но помеченные занятыми блоки.

---

### SFS-7. Таймаут virtio бросает in-flight запрос

**Где:** `src/virtio_blk.rs:309-322`.

**Суть:** по `spins > 50_000_000` функция возвращает `false`, не меняя
`last_used` и не сбрасывая устройство. Слот остаётся в кольце. Следующий
запрос переиспользует дескриптор, но `used.idx` уже сдвинут — чтение
`used`-элемента берёт completion от старого запроса (stale `id`), статус и
данные принадлежат чужой операции.

**Фикс (fail-stop):**
- `static FAILED: AtomicBool`;
- при таймауте: `FAILED = true`, вернуть `false`;
- в начале `read_sector`/`write_sector`: `if FAILED { return false; }`;
- один раз напечатать `VIRTIO: device stalled; filesystem is now read-only`.

Полный reset (`status = 0` → ACK|DRIVER|FEATURES_OK|DRIVER_OK, пересоздание
очереди) — отдельным коммитом после fail-stop, с selftest-хуком.

**Приёмка:** тест с искусственным «молчащим» устройством: первый запрос
`false`, все последующие `false`, ни одной записи в кольцо после таймаута,
нет паники. FS-операции возвращают `EIO`.

---

### MMU-1. Новые L2/L3 линкуются без clean занулённого содержимого

**Где:** `src/mmu.rs:1665-1712` (`user_mmio_table`, обе ветки — L2 и L3).

**Суть:** таблица аллоцируется, `zero_page(l3)`, регистрируется, затем
сразу `write_entry(l2_mmio, index, l3 | DESC_TABLE)` +
`clean_entry_line(l2_mmio, index)`. Чистится **только дескриптор-линия**
родителя; занулённое содержимое новой таблицы остаётся в D-cache и до PoC не
доходит. Walkер на реальном CPU может прочитать из памяти старые (мусорные)
строки → ложные трансляции/фолты.

Это ровно тот сценарий, от которого защищает kernel-stack-арена
(`mmu.rs:544-551`): `zero_page` → `flush_after_table_change(new, GRANULE)` →
только потом линковка. Новый код ритуал не соблюдает. На QEMU невидимо
(кэши не моделируются).

**Фикс:** в обеих ветках `user_mmio_table`:

```rust
zero_page(l3);
flush_after_table_change(l3, GRANULE); // содержимое → PoC до публикации
...
write_entry(l2_mmio, index, l3 as u64 | DESC_TABLE);
clean_entry_line(l2_mmio, index);
finish_table_changes_for(ttbr0);
```

То же для L2 (`l2` → `flush_after_table_change(l2, GRANULE)` перед
`write_entry(l1_user, ...)`).

Дополнительно проверить остальные новые аллокации таблиц в дельте
(`create_task_space` уже корректен — там `zero_page` всех страниц до
публикации).

**Приёмка:** code review + комментарий-инвариант у функции; на QEMU
поведение не меняется, selftest зелёный.

---

## P3 — Low / гигиена

| ID | Где | Фикс |
|---|---|---|
| MMU-2 | `mmu.rs:1236-1245` | unmap: сначала `flush_after_table_change`, потом `free_page_tagged` — как в `destroy_task_space` |
| MMU-3 | `mmu.rs:1636-1647` | write-phase `?` → откат уже записанных PTE (или собрать список и откатывать на ошибке) |
| SCHED-1 | `scheduler.rs:3466-3507` | после `push_migrate` отправить reschedule-IPI целевому PE (`send_ipi(target, RESCHEDULE)`) |
| W-1 | `watchdog.rs:187-192` | читать `current_task_of` (atomic-зеркало) вместо `current_index_of` |
| SIL-3 | `gpu_driver.rs:116` | реестр клиентов: при `try_send` == fail сбрасывать `UI_CLIENT` в 0 (после SIL-1) |
| CACHE-1 | `cache.rs:120-122` | `dc isw`: шаг по `ways`, а не по `size/ways`, либо честно ограничить pow2 и убрать врущий комментарий |
| I-5 | `main.rs:1553` | комментарий: «SFS mounted read-write» |
| I-1 | `virtio_blk.rs:claim_device` | программировать BAR4 через PCI config и проверять, что прочитанный адрес == `BAR4_PA`; иначе fail-stop |
| I-2 | `virtio_blk.rs` init | `queue_size = min(device, QUEUE_SIZE)`, отклонять 0 и > 256 |
| I-3 | `sfs.rs:210` | `free_blocks`/`free_inode`: гейт `block >= data_block_start && block < total_blocks` |
| I-4 | `sfs.rs read_at` | `offset as u32` — проверять `offset <= u32::MAX` до каста |
| SCHED-2 | `scheduler.rs` fallback-скан | при усыновлении idle-задачи проверять owner/space, иначе оставлять в очереди |

---

## Тест-план (обязательный минимум после P0/P1/P2)

**Host (`cargo test --lib`, цель ≥ 182 + новые):**
1. `sfs_corrupt_inode_blocks_do_not_panic` — SFS-1/3.
2. `sfs_append_reuses_blocks` — SFS-2.
3. `sfs_superblock_rejects_bad_metadata` — SFS-4.
4. `sfs_crash_after_inode_flush_no_crosslink` — SFS-5.
5. `virtio_timeout_poisons_device` — SFS-7.

**QEMU:**
- `-smp 1` и `-smp 4`, диск `disk.img`, boot до `howl>`;
- `-smp 4` + `smp-selftest`: remap sentinel / ready fifo / priority /
  TLB cross-PE / watchdog / cache / bkl stall / quiesce / ticks / clock — PASS;
- FS-стресс: 4 таска × 200 операций create/write/append/read/unlink,
  затем offline-проверка `disk.img` (bitmap vs inode, нет общих блоков);
- композитор: SilverHome рисует, тап открывает Calc/Gallery/Notes, ввод в
  Notes работает; поддельный draw/input от тестового таска игнорируется.

## Definition of done

- все P0/P1 закрыты, P2 — закрыты или явно отложены с записью в отчёте;
- новые тесты в `#[cfg(test)]` рядом с кодом;
- `cargo test --lib` зелёный, `./build_and_qemu.sh --no-run` зелёный;
- в `docs/ARCHITECTURE.md` обновлён раздел про SFS: порядок флаша, лимит
  direct-блоков, fail-stop virtio;
- отчёт в конце: что исправлено, чем подтверждено, что осталось.
## Current status (2026-11-08)

The historical findings below remain preserved as an audit trail. The current
tree has since moved beyond the original userspace storage-server design:

- SFS is kernel-owned, mounted read-write at `/sfs`, and backed by the
  synchronous in-kernel virtio-blk path.
- Files support direct, single-indirect and double-indirect block trees;
  positioned writes, directory growth/shrink and offline fsck are covered by
  regression tests.
- App bundles use manifest write scopes (`write=/prefix`). Dynamic ELFs start
  without global rights; capability delegation separates target rights from
  process-wide rights.
- Per-task open-file quota is 32 descriptors, global VFS quota is 128; task
  teardown closes descriptors and releases address-space/DMA/heap/mmap/capability
  resources.
- `SYS_MEMINFO_V2` exposes versioned resource diagnostics without changing the
  legacy `SYS_MEMINFO` ABI.
- Current host test baseline: 199 passing tests. QEMU SMP self-tests and the
  `no_std`, `fog`, `fog-reloc` and UI-related builds remain required before
  merging kernel changes.

Remaining security work includes SFS per-bundle inode/block quotas, metadata
journaling, surface/PRESENT fencing and signed ELF bundles.
