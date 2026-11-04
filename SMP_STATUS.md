# SMP Status — TASK №6/№7 (2026-11-01, сессия 3)

> **Current project status (2026-11-04):** this file remains the authoritative
> SMP investigation record. The Redmi 10C / fog port is single-core and is
> documented in [`redmi10c_poc/README.md`](redmi10c_poc/README.md). The host
> test suite is now **156/156**; the 13/13 figure below is the checkpoint from
> the earlier SMP session.
>
> **Phases 1–2 are implemented as the opt-in `smp` feature (QEMU only).**
> Default builds remain single-core. With `no_std,smp` and QEMU `-smp 4`,
> CPU1–CPU3 are brought online through the physical AP trampoline, their
> GICv3/GICv2 PPI and timer are initialized, they receive IPIs, and they
> execute tasks on every PE. Slots the machine does not expose fail `CPU_ON`
> and stay offline instead of aborting bring-up. The scheduler keeps one idle
> context per CPU; AP idle ids live above the user id space so `SCOUT_ID` and
> spawn-order identities do not move.
>
> Package E2 (owner-aware page-table walks, DRAM-bounded revoke, `CTR_EL0`
> cache-line helper) was written for the single-CPU model. AP task execution
> now works on QEMU, but the space-quiescence argument is still open: teardown
> and DMA revoke have not yet proven that no CPU can hold a translation for a
> space being freed. Keep kill/reap-under-load and physical SMP in the
> unproven column.

## 2026-11-05 — FIFO ready queues and per-CPU stall watchdog

Two scheduler/SMP packages landed in `35c44b4`, on top of the reservation fix
`a8e948e` and the audit round `af23b29`.

- **Ready queues are FIFO.** `TaskManager.ready` was declared FIFO but was a
  `Vec` popped from the back, i.e. a stack: with three or more runnable tasks
  the oldest entry was never dispatched (`A,B,C` ran `C,B,C,B,...` and `A`
  starved until it blocked). It is now a `VecDeque` with
  `push_back`/`pop_front`. `run_ready_queue_selftest` spawns three probes with
  explicit ids (`next_id` saved/restored), pops them through the real
  `dequeue_ready`, restores the queue snapshot and requires spawn order:
  `SMP SELFTEST: ready fifo PASS (3 probes in spawn order)`. Switching the
  pop back to `pop_back` reports `positions=2,1,0` and fails the boot, so the
  test is a real regression guard.
- **Per-CPU stall watchdog.** A PE that has entered the scheduler loop must
  take its timer PPI or complete a scheduling pass every 250 ms check round.
  Three consecutive rounds without progress make any other PE print
  `WATCHDOG: CPU<n> stalled (beats=... index=... rounds=3)` once per episode;
  it does not panic, because a long IRQ mask can recover. The decision lives
  in the host-tested `silver_microkernel::watchdog_policy`. Exemptions: PEs
  that have not called `cpu_started` (AP bring-up, `ap_tlb_probe_wait`), a
  stop-the-world quiesce (all grace windows are rebased), and a CPU parked in
  `bkl_acquire` (`BKL STUCK` owns that condition).
- **Evidence.** `-smp 4` selftest: `WATCHDOG: policy PASS`,
  `WATCHDOG: heartbeat PASS (4 CPU(s) ticking)`, `ready fifo PASS`, plus the
  existing `tlb cross-PE`, `quiesce` (64 rounds), `ticks` and `clock` PASS
  lines. Combined `demo-tasks,smp-selftest` is **74 PASS / 0 FAIL / 0 PANIC**
  with no invariant failure.
- **Negative test (opt-in `smp-churn`).** A kernel task masks IRQs and spins
  for 1.2 s; an AP picks it up and another PE reports it
  (`WATCHDOG: CPU3 stalled (beats=32 index=20 rounds=3)` — the index is the
  probe task), then the task is retired (`watchdog stall PASS`). The stall is
  detected on different APs across runs (CPU1 and CPU3 observed). Disabling
  the check call turns the result into
  `watchdog stall FAIL (reported=0 cleaned=1 pending=0)`.
- **Coverage limits.** QEMU only; no physical-SMP run of this HEAD. The
  watchdog detects a PE that stops making scheduler/timer progress; it does
  not prove device quiescence, and it deliberately does not report a bounded
  BKL wait.

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

> **Историческая заметка (чекпойнт 2026-11-01).** Тогда ядро на `-smp 2` не
> проходило первый блокирующий IPC-приём silverd; разделы «ОТКАЗЫ» ниже
> описывают расследование слоёв A/B. С тех пор это исправлено (см. Phase 1–4.3
> выше: QEMU `-smp 4` работает), а диагностические пробы M1–M8, S1/S2, `SYS sp=`
> и C1–C3 из дерева удалены.

## PHASE 1 (2026-11-03): AP bring-up, QEMU opt-in

Landed behind `--features no_std,smp`; default builds keep `cpu_count() == 1`.

- **PSCI CPU_ON was wrong twice.** The old call used `hvc #0` with function ID
  `0x84020002` (not a PSCI ID) and returned `-1`. QEMU `virt` at EL1 uses the
  HVC conduit with the SMC64 CPU_ON ID `0xC4000003`; it now returns `0`.
- **AP entry is a physical trampoline.** PSCI starts the AP with the MMU off
  at a physical address. The old `ap_entry` branched to a high-half Rust
  symbol and never passed a stack. The new trampoline is position-independent,
  takes a physical `ApBootParams` block in PSCI context-id (x0), loads
  TTBR0/TTBR1/MAIR/TCR, enables FP/SIMD (`CPACR_EL1 = 0x300000`; the Rust path
  contains compiler-generated NEON), enables the MMU and jumps to the
  high-half Rust entry.
- **AP stack and identity page.** CPU bring-up allocates a zeroed 8 KiB stack,
  publishes its high alias, and replaces the AP space's empty user-image L3
  with one EL1-only 2 MiB identity block so the trampoline can execute at its
  physical PC until the branch to high memory.
- **IPIs re-enabled.** `ipi_reschedule_others()` issues SGI0 when more than one
  CPU is online; AP GIC init enables the banked SGI/PPI.
- **Scheduler safety fixes that remain valid for Phase 2:**
  `find_next_task()` skips tasks with `running_on.is_some()` (a task can be
  transiently Ready while still executing on another CPU between
  `schedule_released` and its context switch), and self-continuation restores
  `Running` instead of leaving the current task Ready.
- **Phase 1 checkpoint:** APs were brought online with task execution parked
  (`AP_TASK_SCHEDULING=false`). Phase 2 below enables task execution and fixes
  the idle-context problem that blocked it.

Verified (phase-1 snapshot): `cargo test --lib` 138/138; default `no_std` on
QEMU `-smp 2` stays single-core and reaches `howl>`; `no_std,smp` on QEMU
`-smp 2` shows `SMP: CPU1 online` and reaches `howl>`; 60 s run has no
panic/BKL-stuck; fog QEMU integration remains PASS. No physical hardware SMP.
Later phases raised the same profile to four CPUs; see Phase 4.3.

### Phase 2: AP task execution (working on QEMU)

`AP_TASK_SCHEDULING` is now true in `smp` builds and the AP executes tasks.

- **Shared-idle livelock fixed.** With one global idle task, a blocked CPU0
  could not switch away when CPU1 owned that idle context; it spun through
  `receive_matching`/`schedule_released` with IRQs masked. There is now one
  idle context per CPU.
- **The first per-CPU-idle attempt broke the shell for a different reason:**
  spawning AP idle tasks sequentially shifted `SCOUT_ID` from 2 to 3, so the
  shell's nameserver RPC waited forever. AP idle tasks are now spawned with
  ids above the user space (`1 << 40 + cpu`) and an explicit `Task.is_idle`
  flag; boot-task ids and `SCOUT_ID` stay unchanged.
- `SYS_TASK_LIST` hides AP idle tasks; boot idle 0 remains visible for
  compatibility.

Verified on QEMU `-smp 2`: CPU1 online, both CPUs schedule tasks, `howl ready`
and the prompt appear, `ps` works, and a 60 s run has no panic/BKL-stuck.
Remaining before calling SMP production-ready: space quiescence for
teardown/DMA revoke, TLB shootdown under load, kill/reap stress, physical
hardware validation.

### Phase 3: conservative space-quiescence gate

The first quiescence step is implemented without a stop-the-world protocol:

- `scheduler::space_is_running(ttbr0)` scans every online CPU's current task;
  `destroy_task_space` refuses and leaks a space whose root is still current on
  a CPU.
- `mmu::range_mapped_by(ttbr0, base, end)` performs a read-only, owner-checked
  walk of the MMIO window. It returns `true` for a mapping or for any
  malformed/foreign table (fail-closed).
- DMA reclamation checks all running tasks before clearing the first PTE. If a
  running CPU still maps the range, the whole range is deferred and its frames
  are leaked (`DMA_LEAKED_FRAMES`); otherwise the existing per-space revoke
  proceeds.

This closes the obvious "revoke underneath a running user context" hole, but
it is a **conservative leak policy**, not a full quiescence proof. In-flight
user accesses, a stop-the-world/IPI handshake and TLB shootdown stress remain
Phase 4 work.

### Phase 4: stop-the-world IPI quiescence

The conservative gate now has a real stop-the-world fast path:

- SGI1 (`QUIESCE_IRQ`) is enabled on every CPU. Its handler issues `dsb sy`,
  records a per-CPU acknowledgement for the current round and parks until the
  reaper releases that round.
- Kernel code runs with IRQs masked, so `bkl_acquire` waiters cannot take
  SGI1; their retry loop checks the same request and parks/acknowledges there.
- `smp::quiesce_begin()` (called with the BKL held) sends SGI1, waits for every
  online CPU to acknowledge, and times out fail-closed. `quiesce_end()` releases
  the parked CPUs and kicks rescheduling.
- `reap_dead_tasks` stops the world only when a dead task actually owns DMA
  frames; on timeout it falls back to the conservative per-range leak gate.

This gives a real no-user/no-kernel window on all other PEs before PTE edits
and frame frees. TLB shootdown stress, in-flight-device DMA and physical SMP
remain open.

### Phase 4.1: audit hardening (2026-11-04)

The external audit's SMP-adjacent findings and the remaining INFERRED items
were closed:

- `remap_current_indices` preserves `usize::MAX` ("no current task") instead
  of mapping a vanished task to idle index 0, so an AP cannot get stuck in the
  guard path after a task dies during bring-up.
- An AP is published online only after `gic::cpu_init()` proves a usable
  redistributor/CPU interface and readback of its banked SGI0/SGI1 enables.
  If that fails, the AP parks with IRQs masked and stays offline: it can never
  make quiescence wait for an SGI it cannot receive.
- Late `boot_secondary_cpus` failures (`install_ap_identity`, `CPU_ON`) now
  release the freshly built task space, private page and stack instead of
  leaking them.
- GICv3 bound-IRQ dispatch probes and wakes inside **one** BKL window, matching
  GICv2; an unbind can no longer land between the probe and the wake and make
  the interrupt disappear.

Verified with the `demo-tasks` regression on QEMU `-smp 2`: **66 PASS /
0 PANIC**, including concurrent spawn/kill/reap and DMA revoke while both CPUs
schedule. A forced `gic::cpu_init()` failure parks CPU1 offline and still
reaches `howl>`, proving the failure branch cannot hang quiescence.

The opt-in `smp-selftest` feature runs real SGI1 stop-the-world rounds once a
secondary is published and logs `SMP SELFTEST: quiesce PASS`; it exercises the
same acknowledgement path (including BKL waiters) that DMA teardown uses.
This is functional stress coverage, not a TLB-shootdown proof.

### Phase 4.2: round-qualified quiescence and repeated selftest (2026-11-04)

The boolean `active`/`release`/`ack` triple was replaced by monotonic round
counters:

- `QUIESCE_ROUND` is bumped by each `quiesce_begin`; `QUIESCE_RELEASE` names
  the highest released round; `QUIESCE_ACK[cpu]` (a monotonic `fetch_max`)
  names the highest round that CPU vouched for. `quiesce_active()` is derived
  as `release < round`, so a timeout cannot leave a separate flag disagreeing
  with the parked CPUs.
- A CPU that is still parked when the next round starts now acknowledges the
  new round from inside its wait loop instead of being stranded by a cleared
  boolean. Previously a CPU descheduled across `quiesce_end` → `quiesce_begin`
  could miss the release and make the next round time out, falling back to the
  leak gate and dropping DMA frames. The window is narrow and did not fire in
  QEMU, but it was unbounded in the protocol.
- `smp-selftest` now runs **64 back-to-back rounds** (no yield between them)
  and checks memory visibility, not just arrival: the boot CPU publishes a
  generation before each request, every acking CPU reads it after its own
  `dsb sy`, and the boot CPU verifies the observation while the world is still
  stopped. A missed generation or a timed-out round fails the log line with the
  round number.

Verified on QEMU `-smp 2`, three consecutive boots:
`SMP SELFTEST: quiesce PASS (64 rounds, 1 peer CPU(s))`.

### Phase 4.3: four-core bring-up (2026-11-04)

`cpu_count()` now reports `MAX_CPUS` (4) on QEMU, and the remaining pieces were
made count-agnostic:

- `boot_secondary_cpus` keeps probing after a failed `CPU_ON` instead of
  aborting the loop, so `-smp 2` / `-smp 3` log the missing slots as offline
  and still reach the self-test. `EXPECTED_CPUS` records how many CPUs PSCI
  actually accepted, and the self-test waits for exactly that set.
- AP slot assignment stays independent of the PSCI index: each AP receives its
  own `ApBootParams` and claims the first free `CPUS` slot, so concurrent
  CPU2/CPU3 starts cannot corrupt each other's stack or space.
- GICv2 `GICD_SGIR` (`all but self`, filter `0b01`) and GICv3 `ICC_SGI1R`
  (`IRM` set) already broadcast to any CPU count; the per-PE GIC/timer init
  was not CPU1-specific.
- The kernel console takes a bounded best-effort writer lock so bytes from
  different PEs cannot interleave *inside* one write call. Whole diagnostic
  lines can still interleave at call boundaries; the self-test lines are
  printed by the boot CPU after the APs have stopped logging.
- `timer` now keeps a per-CPU tick counter alongside the boot-CPU-only global
  `TICKS`. `smp-selftest` waits for every online PE to advance its own counter,
  which distinguishes a live core from one that is online but no longer taking
  its PPI.
- The 4-core demo regression passes with the same suite.

Verified on QEMU `-smp 4` (GICv2 `virt`, cortex-a72):
`SMP SELFTEST: quiesce PASS (64 rounds, 3 peer CPU(s))` and
`SMP SELFTEST: ticks PASS (3 peer CPU(s))`, then `howl>`;
`no_std,smp,demo-tasks -smp 4` exits **66 PASS / 0 FAIL / 0 PANIC** with no
DMA leak, and `-smp 2` still reports one peer.

## Что сделано (фазы 1–6 задания TASK №6)

| Фаза | Содержание | Состояние |
|---|---|---|
| 1. Примитивы | `src/smp.rs` (новый): `SpinLock<T>` (IRQ-save + asm-спин), per-CPU массив `CPUS[4]` через `TPIDR_EL1`, Big Kernel Lock (atomic owner + per-CPU depth, реентерабельный), `BklGuard`, IPI-хелпер | написано, собирается |
| 2. Планировщик | `current_index` → per-CPU; `Task.running_on` (Some ⟺ Running); reap скипает `Dead && running_on.is_some()`; `schedule()` под BKL; `schedule_released()` для всех блокирующих путей; wake → reschedule-IPI; per-CPU ready queues `(id, index)` с local→steal→scan порядком | написано, очереди проверены на `-smp 4` |
| 3. Аллокаторы | frame allocator `STATE` → `SpinLock`; `PageTableRegistry` → `SpinLock` | написано |
| 4. Синхро-примитивы | Mutex/WaitQueue под kernel lock, `bkl_assert_held()` в точках мутации, дока обновлена | написано |
| 5. IRQ/таймер | gic: `init`/`cpu_init` разделены (banked SGI0+PPI30 в `cpu_init`), `send_sgi_all_but_self` (GICD_SGIR); gicv3: **поиск redistributor по `GICR_TYPER`**, ICC_SGI1R; таймер per-CPU (`cpu_init`), глобальный `TICKS` инкрементирует только CPU0 | написано |
| 6. TLB | per-space ASID (`1..=255`, 8-bit) в `TTBR0[63:48]`; `switch_address_space` только `msr`+`isb`; правка PTE — `tlbi aside1is` по ASID; создание/AP-identity — `vmalle1is`; батчевая публикация (`finish_table_changes_for`); cross-PE probe доказывает, что ASID-scoped broadcast доходит до второго PE; boot-тест ASID-изоляции | написано, проверено на QEMU `-smp 4` |
| 7. AP-boot | PSCI `CPU_ON` (HVC, `0xC4000003`), physical trampoline, `ApBootParams`, FP/SIMD + MMU enable, `boot_secondary_cpus()` from the first `schedule()` | работает на QEMU `-smp 2` и `-smp 4` |
| 8. Shared DMA | под BKL: создание маппингов (syscall) и revoke/teardown взаимоисключены; fail-safe leak сохранён; `DmaLease` in-flight gate: диапазон с живым lease не освобождается | написано, boot-тест `DMA LEASE` |
| 9. Тесты | `demo-tasks` (66 PASS) на `-smp 2`/`-smp 4`; `smp-selftest` — cross-PE TLB probe + 64 раунда quiesce + visibility + per-CPU timer + проверка `running_on`/`current_index` на каждом проходе + FIFO-порядок очередей + stall-watchdog policy/heartbeat; `demo-tasks,smp-selftest` — 74 PASS / 0 PANIC / 0 invariant FAIL; boot-самотесты MMU TLB, MMU HEAP, DMA ZERO, UART RING, CONSOLE CAP и DMA quarantine | написано, негативные тесты подтверждены |

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

## Что дальше (Phase 5)

1. **TLB/DMA under SMP.** Re-audit `flush_after_table_change`,
   `switch_address_space` and `revoke_phys_range` against a second PE holding a
   stale translation; keep `safe_to_free` CPU-only and retain the leak policy.
   *Done:* a boot self-test now switches the running PE to a scratch TTBR0,
   reads a value, repoints the same VA at another frame and reads again. It
   fails the boot if the `tlbi ...is` in `flush_after_table_change` is removed
   (verified by a negative build: exception 9). A second, **cross-PE** probe
   covers the gap that test leaves: the APs are parked before the scheduler
   loop, switch to the boot CPU's scratch space, read the VA, wait while the
   boot CPU re-points the same PTE, then read it again. With the broadcast
   `tlbi vmalle1is` replaced by a local `tlbi vmalle1`, all three peers keep the
   stale translation and the probe reports FAIL, so the test is sensitive to a
   non-broadcast flush on a *different* PE.
   Table publication is now batched (`finish_table_changes`, one shootdown per
   map/heap-growth call) in the XNU `pmap` "edit the range, flush once" style.
   *ASID follow-up:* every space now owns a unique ASID in `TTBR0[63:48]`.
   `switch_address_space` writes TTBR0 and executes `isb` only — no
   domain-wide flush on a context switch — and a PTE edit uses
   `tlbi aside1is` for the edited ASID (still a broadcast within the
   inner-shareable domain). `destroy_task_space` invalidates the ASID before
   the tables are retired and returns it to a 255-entry live bitmap. A boot
   test maps the same VA in two spaces and switches between them without a
   flush; dropping the tag makes it fail closed.
2. **Stress tests.** Multi-CPU timer/IPI, concurrent spawn/kill/reap, shared
   DMA revoke while a client runs, and page-table teardown under load. The
   `demo-tasks` suite passes on `-smp 2` and `-smp 4` (66 PASS, 0 PANIC);
   `smp-selftest` runs 64 back-to-back stop-the-world rounds with a per-round
   visibility check and a per-CPU timer-liveness check. The cross-PE TLB probe
   above proves one remap crosses the domain, but an in-flight-MMAP stress
   (a task actively faulting while its space is edited) still needs a task
   pinned to a shared space, which the current one-space-per-task model does
   not provide. Every scheduling pass in `smp-selftest` builds also verifies
   the `running_on`/`current_index` pairing across CPUs after the reap remap;
   the `demo-tasks,smp-selftest` profile exercises that under the full
   regression (69 PASS / 0 PANIC, no invariant failure). Dispatch now goes
   through per-CPU ready queues: local queue, then steal one task from a peer,
   then the old full scan as a fallback. Entries carry `(id, index)`, so an
   index shift can only discard an entry; the removal paths rebuild every
   queue. The counters report `local`/`steal`/`scan`, and a corrupted-id
   negative build trips the invariant audit.
   **`smp-churn` harness (opt-in).** 12 short-lived kernel tasks are
   dispatched, yielded/slept and reaped concurrently while the boot CPU waits
   with the lock released. It found and now regression-tests three scheduler
   bugs: (a) a scheduling pass was preemptible, so a timer IRQ could run a
   second pass on a half-applied index snapshot and an outer pass later
   republished the stale index; (b) kernel-task `yield`/`sleep` released a
   kernel lock they never held, wrapping the per-CPU depth to `u32::MAX`;
   (c) an IRQ between publishing the lock owner and setting the depth could
   re-enter `schedule` and deadlock. The pass now masks IRQs and refuses a
   nested entry, `yield`/`sleep` take the lock when needed, the lock
   acquisition is IRQ-atomic, and a release without an acquire fails safe
   instead of wrapping. Stable across repeated `-smp 4` runs with the
   invariant audit active; kept opt-in because `cpu_count()` assumes
   `MAX_CPUS` and machines with fewer online CPUs fail the harness policy.
3. **Device DMA quiescence.** The stop-the-world window covers CPUs, not a
   device with an armed queue; the driver stop/reset contract stays
   authoritative. *Done (kernel side):* revoked runs are quarantined for
   `DMA_QUARANTINE_EPOCHS` scheduling passes (~80 ms) before their frames
   return to the allocator, and a boot self-test proves the window. This bounds
   the post-revoke reuse window; it is not an IOMMU or a device-reset proof.
   *Lease gate:* `DmaLeaseTable` lets an in-kernel driver declare a range
   in-flight; revoke leaks any range with a live lease instead of quarantining
   it. The boot `DMA LEASE` test proves the overlap gate, and disabling it
   fails the boot. Userspace drivers (`block_driver`) cannot arm a lease yet —
   that needs a new syscall — so their ranges still use the time window.
4. **Hardware.** Fog/tablet stay single-core; physical SMP remains unproven.


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
  (оставить механизм, значение `false`); `AP_TASK_SCHEDULING` — runtime gate,
  в `smp`-сборке `true`.
- `src/allocator.rs`: `HEAP_PAGES` 512 (бисект) — вернуть 256.

## Проверки на текущем состоянии

- `cargo test --lib`: **156/156**.
- Default `no_std` build, QEMU `-smp 4`: single-core, reaches `howl>`.
- `no_std,smp` build, QEMU `-smp 4`: CPU1–CPU3 online (GIC/timer/IPI), all
  four CPUs execute tasks; `howl ready`/prompt and `ps` work.
- `no_std,smp` build, QEMU `-smp 2`: only CPU1 comes online; CPU2/CPU3 report
  `CPU_ON failed` and stay offline without aborting the boot.
- `no_std,smp,demo-tasks`, QEMU `-smp 4`: **66 PASS / 0 FAIL / 0 PANIC**,
  clean exit, no DMA leak; spawn/kill/reap and DMA revoke ran while all four
  CPUs were scheduling. The same suite passes on `-smp 2`.
- `no_std,smp-selftest`, QEMU `-smp 4`: `SMP SELFTEST: tlb cross-PE PASS (3
  peer CPU(s) saw the remap)`, then `quiesce PASS (64 rounds, 3 peer CPU(s))`
  and `ticks PASS (3 peer CPU(s))`, then `howl>`; `-smp 2` reports one peer.
  Negative build (broadcast `tlbi vmalle1is` replaced by local `tlbi vmalle1`
  in `finish_table_changes`): `tlb cross-PE FAIL (3 CPU(s) kept the stale
  translation)` while quiesce/ticks still pass.
- `no_std,smp,demo-tasks,smp-selftest`, QEMU `-smp 4`: **74 PASS / 0 FAIL /
  0 PANIC** with zero `sched invariant FAIL`; the extra PASS lines are the
  selftest lines (FIFO order, stall-watchdog policy and heartbeat, remap
  sentinel, cross-PE TLB, quiesce, ticks, clock). The dispatch-mix line reports
  the per-CPU queues doing the work (`ready queues local=.. steal=.. scan=..`;
  selftest-only reports more `scan` because idle tasks are deliberately not
  queued). Negative build
  (queued entry id deliberately corrupted): `sched invariant FAIL (a ready
  queue entry names the wrong task)` followed by `PANIC MSG scheduling
  invariant violated`.
- `no_std,smp,demo-tasks,smp-selftest`, QEMU `-smp 4`: negative build (the
  `running_on` claim removed in `schedule`): `sched invariant FAIL (a CPU's
  current task is not marked running on it)` followed by `PANIC MSG
  scheduling invariant violated`.
- Forced AP GIC failure (temporary build-only patch): CPU1 stays offline,
  prints `AP GIC unavailable`, and the system still reaches `howl>`.
- Boot self-tests: `MMU TLB OK` (TTBR0 remap proof), `MMU ASID OK` (two spaces,
  same VA, switched without a flush, each read returns its own frame),
  `DMA QUARANTINE OK` (grace-window proof) and `DMA LEASE OK` (in-flight gate
  proof) run on every boot. Removing the `tlbi` in `flush_after_table_change`
  makes the TLB test stop the boot with exception 9; dropping the ASID tag from
  `tagged_ttbr0` makes the ASID test stop the boot with exception 9; disabling
  `DmaLeaseTable::range_busy` makes the lease test stop the boot with
  exception 9.
- `no_std,fog` / `no_std,tablet`: build; fog QEMU integration PASS.
- Physical SMP: **not verified**; fog/tablet remain single-core.
