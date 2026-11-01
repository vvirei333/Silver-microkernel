# SMP Status — TASK №6/№7 (2026-11-01, сессия 3)

## CHECKPOINT (финальные проверки)

- TASK №7 частично выполнена: исправлены сырой доступ к `tasks`
  (`schedule()`), aliasing в `schedule()` (`manager` после `as_mut_ptr()`),
  BKL-release (восстановлен `bl bkl_release_asm` в `silver_user_bootstrap` +
  добавлен release после возврата из переключения контекста) и bisect-bypass
  в `ipc.rs` (удалён, настоящий wait-путь восстановлен).
- `cargo test --lib`: **13/13**.
- Debug build и release build проходят.
- Проблема O3+LTO (компиляторные self-loop `b .` на достижимых путях,
  ~22 сайта) пока **не имеет окончательно установленной первопричины**;
  при `opt-level=1` self-loop'ов нет.
- O1-прогон выявил **Prefetch Abort**: ESR `0x86000006`, FAR `0x40081a00`
  (векторная страница становится нефетчабельной в ходе работы).
- Повреждение таблиц трансляции остаётся неисследованным (кандидат на
  первопричину; см. раздел «ОТКАЗЫ»).
- SMP не подтверждён: CPU1 offline (PSCI CPU_ON возвращает `-1`, AP-boot
  выключен бисектом).
- Ядро нельзя считать готовым к аппаратной загрузке.
- **Интерпретация значения `1` в `owned_mutexes` (ранняя диагностика):
  НЕ считать доказанным повреждением памяти.** Позднейшая диагностика
  показала, что это значение скорее всего было следствием скомпилированных
  self-loop'ов (слой A): рангтайм попадал в недостижимые по LLVM ветки,
  и регистры при замерах могли не отражать реальные данные списка.

**ВНИМАНИЕ: незавершённая работа. Ядро на `-smp 2` не проходит первый блокирующий
IPC-приём silverd. Диагностические RAM-пробы и serial-принты (M1–M5, S1/S2,
SYS sp=, C1–C3) СОХРАНЕНЫ сознательно — они нужны для охоты на писателя кадра
0x40081000 (слой B).**

## Что сделано (фазы 1–6 задания TASK №6)

| Фаза | Содержание | Состояние |
|---|---|---|
| 1. Примитивы | `src/smp.rs` (новый): `SpinLock<T>` (IRQ-save + asm-спин), per-CPU массив `CPUS[4]` через `TPIDR_EL1`, Big Kernel Lock (atomic owner + per-CPU depth, реентерабельный), `BklGuard`, IPI-хелпер | написано, собирается |
| 2. Планировщик | `current_index` → per-CPU; `Task.running_on` (Some ⟺ Running); reap скипает `Dead && running_on.is_some()`; `schedule()` под BKL; `schedule_released()` для всех блокирующих путей; wake → reschedule-IPI | написано |
| 3. Аллокаторы | frame allocator `STATE` → `SpinLock`; `PageTableRegistry` → `SpinLock` | написано |
| 4. Синхро-примитивы | Mutex/WaitQueue под kernel lock, `bkl_assert_held()` в точках мутации, дока обновлена | написано |
| 5. IRQ/таймер | gic: `init`/`cpu_init` разделены (banked SGI0+PPI30 в `cpu_init`), `send_sgi_all_but_self` (GICD_SGIR); gicv3: **поиск redistributor по `GICR_TYPER`**, ICC_SGI1R; таймер per-CPU (`cpu_init`), глобальный `TICKS` инкрементирует только CPU0 | написано |
| 6. TLB | `tlbi vmalle1is` + `dsb ish` в `flush_after_table_change`/`switch_address_space`/`configure_and_enable`; протокол задокументирован в доке | написано |
| 7. AP-boot | PSCI `CPU_ON` (hvc, 0x84020002), `ap_entry` asm (VBAR, sp), `smp_ap_main` (`mmu::ap_enable` + `kernel_ttbr1()`), `boot_secondary_cpus()` one-shot из первого `schedule()` | написано, **не тестировалось** (заблокировано freeze) |
| 8. Shared DMA | под BKL: создание маппингов (syscall) и revoke/teardown взаимоисключены; fail-safe leak сохранён | написано |
| 9. Тесты | — | **не начинались** |

## Найдено и исправлено попутно (реальные баги)

1. **`current_task_mut` aliasing-UB** (пре-существующий):
   `get_mut(idx).map(|_| &mut *tasks.add(idx))` — второй `&mut` поверх уже
   выданного `get_mut`. LLVM после перехода на per-CPU index превратил
   «невозможные» пути в `b .`. Исправлено: `manager.tasks.get_mut(idx)`.
2. **LLVM удалял пустые spin-циклы**: `while atomic_cmpxchg { spin_loop() }`
   компилировался в literal `b .` (атомарный retry удалялся вместе с циклом).
   Исправлено: непрозрачный asm-спин (`ldaxr/stlxr`) в `SpinLock::lock`,
   `yield`-asm в `bkl_acquire`. **Правило для этого кода: любой спин — через asm.**
3. **build.rs**: userspace-субсборка всегда `--release` — debug-профиль строил
   user-ELF с ~700 КиБ debug-info, переполнявший 1 МиБ heap при VFS-seed
   (это же маскировало debug-отладку). Держать.

## TASK №7 — применённые исправления (2026-11-01, сессия 3)

1. **`scheduler.rs` schedule()**: сырой `(*tasks.as_mut_ptr().add(idx)).running_on = None`
   заменён на bounds-checked `get_mut` с диагностикой; устранено использование
   `manager` (`&mut TaskManager`) после создания `next_task: &mut Task`
   (aliasing-UB по Stacked Borrows — всё после `as_mut_ptr()` только через raw).
2. **`scheduler.rs` schedule()**: добавлен `smp::bkl_release()` после
   `silver_context_switch` — вторая половина контракта smp.rs:19-23 («incoming
   task completes the release»): каждая пара acquire/release замкнута в одном
   кадре стека, заблокированная задача не пинит lock.
3. **`scheduler.rs` silver_user_bootstrap**: восстановлен потерянный
   `bl bkl_release_asm` (sym-operand) перед eret в EL0.
4. **`scheduler.rs` unregister_mutex**: путь not-found — реальный код с
   диагностикой (раньше LLVM сворачивал его в недостижимость).
5. **`ipc.rs`**: бисект-обход `probe(9); return Err(Timeout);
   #[allow(unreachable_code)] {...}` УДАЛЁН — восстановлен настоящий wait-путь.
6. Проверены: 13/13 `cargo test --lib`, `cargo kernel`, `cargo kernel-release`,
   `git diff --check` (кроме пре-существующего pixelos-бинаря).

## ОТКАЗЫ (два независимых слоя, оба новые относительно пре-SMP ca84fc9)

### Слой A: O2/O3 — компиляторные self-loop'ы (`b .`) на ДОСТИЖИМЫХ путях
- Прe-SMP-бинарь (тот же rustc 1.98.1, тот же профиль O3+LTO): **0** self-loop'ов.
- Текущее дерево (O3): ~30 self-loop'ов, **22 достижимы** (raw_lock ×3,
  raw_unlock, Semaphore::wait, Channel send/receive, ipc send/try_send/remove_task,
  sys_receive, wait_inner, stop_console_owner, exit_current, kill_task,
  MutexGuard drop-glue ×3).
- Место фриза: drop-glue(MutexGuard<IpcState>): `str xzr,[IPC.state.owner];
  bl unregister_mutex; b .` — LLVM считает продолжение unregister мёртвым
  (IR: после `tail call unregister` стоит бесконечный self-loop `tailrecurse`),
  хотя unregister_mutex в IR имеет живой `ret void`, и рангтайм дошёл до
  успешного пути retain (элемент найден и удалён, len записан).
- НЕ повлияло: BKL_DISABLED=true, #[inline(never)] (unregister / current_task_mut),
  `-C llvm-args=-disable-tail-calls`, `-C lto=off`, debug-assertions=on.
- Оптимизационно-чувствительно: `-C opt-level=1` — 0 self-loop'ов.
- **Не найдена исходная конструкция, дающая LLVM право рвать пути. Кандидаты:
  aliasing `&'static mut` (current_task_mut / this_cpu), взаимодействие
  loop+block_current_on+panic=abort. Требуется отдельное расследование
  (LLVM-багрепорт-уровень).**

### Слой B: O1 (0 self-loop'ов) — реальное повреждение трансляции ядра
- Система НЕ виснет: silverd проходит M5, уходит в wait, затем CPU0 уходит в
  **бесконечный Prefetch Abort**: ESR 0x86000006 (permission, level 2),
  FAR=ELR=PC=0x40081a00, «to EL1 PC 0xffff000040081a00» (вектор правильно
  указывает на 0x40081a00=`b exception_sync_entry`, НО САМА ЭТА СТРАНИЦА
  0x40081000..0x40081fff стала нефетчабельной В ХОДЕ РАБОТЫ).
- silver_handle_exception НЕ печатает «EL1 EXCEPTION» → исключение из EL1 в
  EL1 ретранслируется на вектор, который сам нефетчабелен.
- x0=0x401fa020 (=&IPC) — повреждение произошло в IPC-обработке.
- Это объясняет и «невозможные» значения (1 в owned_mutexes, теги enum):
  повреждение памяти ядра (таблицы/VBAR/данные) — первопричина.
- **Кто повреждает — НЕ установлен. Проверить при возобновлении: (а) что пишется
  в физ. кадр 0x40081000 (watchpoint через gdb -S + hw-watch на чтение
  дескриптора L2/L3); (б) relocations/copy-этап в main.rs asm (image_size,
  __bss_end) для выросшего с SMP образа; (в) build_task_space: не попадает ли
  кадр таблицы задачи в диапазон kernel image при новых размерах.**

## Что дальше (по разрешению ревьюера)
1. Ловить писателя кадра 0x40081000 (gdb watchpoint на L2-дескриптор
   0x40000000-блока в TTBR1 — таблицы kernel в .bss — адреса из nm).
2. Восстановить симметрию pre-SMP: `git diff ca84fc9 -- src/mmu.rs` по блокам
   (revoke/registry — TASK №5 — сверить, что SMP-правки не задели пути №5).
3. Если слой B чинится — слой A может исчезнуть (повреждённые данные
   делают пути «невозможными» с точки зрения LLVM-модели).


## БЛОКЕР: boot-freeze на `-smp 2` (историческая часть)

- Симптом: silverd доходит до первого блокирующего `SYS_RECEIVE` (~9 мс),
  последняя строка serial — `M7` (инструментация `receive_matching`), `C1`
  (вход в `Condvar::wait`) не печатается; таймерные IRQ не приходят вообще;
  `schedule()` ни разу не выполняется (one-shot print не сработал).
- exec-trace: CPU крутится (~1.7M итераций) в одном TB — `b .` сразу после
  `mrs DAIF / msr DAIFSet,#2` в ipc-регионе (аттрибуция символа врёт из-за LTO).
- Исключено бисектом: BKL полностью выключен → висит; IPI выключен → висит;
  heap 256→512 → висит; opt-level 0/1/3 → висит; debug-сборка → висит молча
  (assert не срабатывает).
- Кандидаты на проверку при возобновлении:
  1. sp syscall-хендлеров = 0x40320EB0/0x40329D70 (менялось между билдами) —
     сверить с `context.stack_pointer = phys_to_virt(stack.end)`.
  2. `bl bkl_release_asm` в `silver_user_bootstrap` — дизасм: доходит ли,
     не портит ли x19/x20.
  3. Окно M7→C1 пустое ⇒ C1 реально не достигается ⇒ проверить
     `bl` в `Condvar::wait` и sp-сдвиг в debug-дизасме.
  4. Boot-stack `__stack_top` (0x4020B630) не помечен в bitmap аллокатора —
     проверить перекрытие с аллокациями спавна.
- gdb-стаб: в этом окружении подключения нестабильны (таймауты), stale QEMU
  держат lock disk.img — чистить `ps` + `/bin/kill -9`.

## Бисект-мусор в дереве (убрать перед сдачей)

- `src/ipc.rs`: prints M1–M8 + sp, `SEND from=` в `send`.
- `src/condvar.rs`: prints C1–C3.
- `src/syscall.rs`: `SYS <n> sp=` трейс.
- `src/scheduler.rs`: `B1/B2` в `block_current_on`, `R1–R4` в
  `schedule_released`, `SCHED_SEEN` в `schedule`.
- `src/smp.rs`: `BKL STUCK`-детектор в `bkl_acquire`; `BKL_DISABLED`-свитч
  (оставить механизм, значение `false`); **`ipi_reschedule_others` выключен
  (`if false`) — ВЕРНУТЬ**.
- `src/allocator.rs`: `HEAP_PAGES` 512 (бисект) — вернуть 256.

## Проверки на текущем состоянии

- `cargo kernel` / `cargo kernel-release`: OK (3 warnings: capability dead_code
  пре-существующий + 2 never-used заготовки SMP).
- `cargo test --lib`: 13/13.
- QEMU `-smp 2`: **FREEZE** (см. выше). Коммит не делать.
