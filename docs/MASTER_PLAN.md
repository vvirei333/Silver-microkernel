# WEREWOLFOS / SILVER KERNEL — МАСТЕР-ПЛАН (2026-11-14)

**Вектор проекта:** консольная ОС = свой базовый слой (микроядро Rust AArch64,
capability-модель, SFS, сеть, Howl) + Linux ABI (userspace-процесс, НЕ VM) +
Phosh. Reference device: **Redmi 10C (SM6225)**. Лицензия MIT. Философия:
форки/копии = валидация. Конечная амбиция: самостоятельная 3-я категория
мобильных ОС + RISC-V.

## ПРИНЦИПЫ ДЛЯ ИСПОЛНИТЕЛЯ (обязательные)

1. Никогда не линковать Linux-специфичное в kernel image (feature boundary
   свято).
2. Ядро не доверяет никому: copy-in/out с валидацией, caps на каждый вызов,
   revocation атомарный.
3. Каждый класс найденного бага = 1 тест в `tests/security/` регрессию.
4. Милстоун > перфекционизм. Известный баг ≠ блокер релиза.
5. Все arch-специфичное — в `arch/aarch64/` за trait-интерфейсом (готовим
   RISC-V).
6. Каждый драйвер = EL0-таск с MMIO/IRQ-капами ТОЛЬКО на свою DT-ноду.

---

## M0. ЗАКРЫТЬ PHOSH-БЛОКЕРЫ (текущее, 1-2 недели)

**M0.1 — CRITICAL stacked-surface** (`phoc-output-CRITICAL: Stacked surface ... not in same layer`)

- Разобрать, как ванильный phoc на postmarketOS обрабатывает стэкинг
  top-panel background + top-panel (у них НЕ фейлит).
- Сравнить namespace/layer у Phosh's `PhoshLayerSurface` — либо обе
  поверхности в одном layer, либо phoc принимает cross-layer.
- Фикс: патч phoc «rejected stack → keep in place» или исправление namespace.
- ✅ Критерий: CRITICAL исчез из лога.

**Статус 2026-11-14:** CRITICAL есть и на реальных устройствах (PureOS/Phosh на
Librem 5, PinePhone — сообщения `pureos phoc: Stacked surface phosh top-panel
background and target phosh top-panel surface not in same layer` при работающем
шелле). Значит это **косметика**, не блокер. Понижено до M0.4/TODO.

**M0.2 — чёрный экран при фокусе в text-entry**

**Статус 2026-11-14 (z-order-лог):** гипотеза «поверх всего лежит локскрин-фон»
и «застрявший output shield» **опровергнуты**. Все layer-surface (включая
`phosh background` layer 0) в чёрном кадре mapped; CSS-пробы и ярко-розовые
обои доказывают, что сцена не перекрыта. phoc сам коммитит **пустой кадр** с
damage на весь выход (source = прозрачный/непрозрачный чёрный), после чего
рисует только курсор. В тот же момент layout-транзакция таймаутится (2
configure pending), затем acks приходят при `pending_layer_configures == 0` —
это и есть CRITICAL. `PHOC_DEBUG=damage-whole` и preload-шим на
`wlr_damage_ring_rotate_buffer` (полный damage каждый кадр) не помогают.
Ввод при этом доходит до Phosh (`wl_pointer.enter/button` на home,
`wl_keyboard.key`) — подвисает отрисовка, не доставка событий.

Вывод: фикс = патч phoc в `phoc_layer_surface_send_configure` (начинать/
продлевать транзакцию при уже висящем `pending_serial`) + защитное опускание
шилда при `pending == 0`. Нужна пересборка phoc 0.57.0 из исходников
(gitlab.gnome.org доступен). До этого — known issue в 0.1.2.

**Статус 2026-11-14 (пересборка):** phoc 0.57.0 собирается в Arch ARM
chroot (`/home/ivan/alarm`), запускается в образе. Инструментация показала:
в чёрном кадре сцена **рисуется полностью** (5 layer-surface, layer 0
`mapped buffer=1 texture=1`, damage 1024x768), но коммит уходит с damage
**1280x768** при буфере 1024x768 — output/damage-ring не совпадает с
swapchain-буфером. Патч транзакции (per-surface `in_transaction`) и
pixman-full-damage чёрный кадр не убирают. Детали и артефакты — в
`handoff.md` (раздел «M0.2 rebuild»).

**Статус 2026-11-15 — ЗАКРЫТО.** Причина оказалась **в прокси, не в phoc**.
`wayland_real.c` закрывал fd shm-пула при переиспользовании слота пула, пока
буферы, созданные из уничтоженного пула, ещё коммитились. Номер fd успевал
переиспользоваться (в логе `fd=0`), прокси мапил чужой файл и получал нули;
full-damage коммит чёрных пикселей отравлял `app_window.copy`, после чего
частичный damage уже не мог восстановить кадр. phoc при этом рисовал и
коммитил правильный кадр (readback пикселей выходного буфера: wallpaper,
`nonblack=3072/3072`).

Фикс: каждый shm-буфер делает `dup()` fd пула (`slot->owned = 1`), fd
закрывается в `buffer_forget`/при переиспользовании слота; пулы снова могут
безопасно закрывать свой дескриптор. Проверено с **оригинальным** phoc:
unlock → tap по полю поиска → ввод текста → результаты поиска, чёрного кадра
нет. Патчи phoc (transaction hardening, pixman-full-damage) остаются
экспериментальными артефактами в `/home/ivan/alarm`, в образ не входят.

- Первичное подозрение: реализация `zphoc_layer_shell_effects_v1` (alpha=0 на
  home background при fold; state не восстанавливается / применяется к output
  вместо surface).
- Добавить лог в compositor: какие alpha-запросы приходят в момент чёрного
  кадра; сравнить с ожидаемой последовательностью Phosh (fader.c/home.c).
- Если баг не найден за 2-3 сессии — **timebox: релизим с known-issue** (см.
  M1).

**M0.3 — host-baseline эксперимент** (дешёвый, 1 сессия)

- Nested phoc под Sway/weston на хосте, тот же phoc.ini. Воспроизводится чёрный
  кадр → баг phoc/wlroots (искать апстрим-issue «stacked surface not in same
  layer»). Не воспроизводится → баг прокси-композитора.

**M0.4 — tech-debt запись**: damage_keep.c = workaround wlroots pixman; проверить
версию wlroots (0.18/0.19 pixman-рендерер доработан), записать в STATUS.md как
debt. Апгрейд только если ребейз патчей ≤ 2 дней.

---

## M1. РЕЛИЗ 0.1.2 (цель: 2-4 недели от M0)

Состав (минимум, не перфекционизм):

- Boot → fastfetch-banner → Phosh (nested phoc) рисует стол с обоями
- Lock screen unlock работает
- App grid показывает тайл(ы), запуск терминала работает
- Тест-матрица STATUS.md зелёная, `cargo test --lib` ≥ 221
- Известные issues — в RELEASE.md с трекингом
- **Демо-видео 60 сек**: boot → стол → свайп → терминал → ls → exit

**M1.5 — чистый публичный репо:**

- Отделить kernel+OS от research-хвостов; README: что работает/что нет,
  фастфетч-скрин, демо-гифка, **Reference device: Redmi 10C (SM6225)**,
  манифест: *"WerewolfOS is free software. Forks, copies and closed derivatives
  are not enemies of this project — they are proof it worked."*
- Trademark-декларация имени, ©-строки, DCO для PR.
- Теги: `silver-kernel-0.1.2`, `werewolfos-0.1.2`.

---

## M2. SECURITY ФАЗА (параллельно с M3, старт после M0)

**M2.1 — docs/SECURITY.md** (2 страницы): протокол «что ядро принимает от
таска» — copy-in/out, дескрипторы, caps, лимиты (таблица из STATUS.md как база).

**M2.2 — CVE-pattern transfer audit** (аудитор готовит surfaces):

- Классы: OOB/int-overflow в парсинге, UAF/double-free на error-path (caps
  revocation — главный кандидат), TOCTOU (cap проверили → объект урезали),
  first-caller-trusts-last, missing clamp у соседних syscall'ов (close_range vs
  dup3 — уже поймано!).
- Первая цель: **syscall-трамплин + IPC-приём** — только 2 точки входа ядра.
- Формат: файлы → аудитор проходит по классовой карте с адресами,
  [уверен]/[предположение].

**M2.3 — фаззер (как только M2.1 готов):**

- Syscall-фаззер: мусорные дескрипторы, гигантские count, отрицательные offset,
  close_range(3,~0U) уже как кейс.
- IPC-фаззер: битые дескрипторы сообщений, stale handles, переполнение mailbox.
- Каждый panic → баг-трекер → fix → тест в `tests/security/`.
- Цель полугода: 30 кейсов регрессии. Каждый найденный баг = пост в TG.

**M2.4 — graduated lockdown (после фаззера):**

- L0 тихий отказ; L1 карантин per-task (счётчик + decay, НЕ глобальный);
  L2 рестарт подсистемы (kill ABI-сервера + атомарный revoke caps + чистый
  перезапуск); L3 panic ТОЛЬКО на kernel-инварианты.
- Детектор = EL0-сервис с правом ТОЛЬКО отзывать caps; детекция по
  фактам-инвариантам (право-нарушение = доказанная атака), не эвристикам.
- Тест выживаемости: `kill -9` Linux-таска под Phosh → ОС жива.

**M2.5 — kernel self-hardening:** W^X, hardened allocator (guard-страницы,
quarantine free, канарейки), checked-арифметика на всех юзер-числах.

---

## M3. ДРАЙВЕРНАЯ МОДЕЛЬ + REDMI 10C (после M1, длительная)

**M3.1 — driver-task framework:**

- Драйвер = EL0-таск; MMIO/IRQ-капы только по своей DT-ноде (ядро мапит MMIO
  ТОЛЬКО по DT — попытка вне ноды = L1-инцидент).
- DT-енумерация: ядро читает DT → device-ноды → драйверы биндятся по
  `compatible`; не подошёл = нода unmapped (мёртвое железо, не падение).

**M3.2 — basic pack** (QEMU → плата): virtio уже есть; добавить 8250-стиль UART,
framebuffer (simplefb: base+stride+формат от бутлоадера), eMMC-блок,
HID-клавиатура, I2C-мини.

**M3.3 — Redmi 10C bring-up (этапность, каждая ступень = демо):**

1. simplefb экран (u-boot/ABL) — **первая картинка на железе**
2. UART/консоль
3. eMMC/UFS (файлы)
4. Touchscreen/I2C
5. Wi-Fi (WCN3988/ath10k-референс)
6. GPU = M3.4

- Каркас DT: mainline `qcom-sm6225` как референс.
- Modem/звонки: НЕ цель, отложить честно.

**M3.4 — GPU (freedreno):**

- Kernel-side: свой аналог msm-DRM uAPI (минимальный путь: GET_PARAM / NEW_BO /
  SUBMIT / sync) + Adreno 610 MMU (a6xx).
- Cmdstream-валидация — central security-витрина (90% CVE DRM живут там).
- Патч Mesa freedreno → silver-бэкенд (~2-3К строк; референс дизайна:
  Fuchsia/Magma).
- НЕ начинать до завершения M1.

**Запрет навсегда:** «переносить Linux-драйверы в ABI», «Linux как VM» — Linux
ABI = userspace ELF-исполнитель, драйверы только свои.

---

## M4. RISC-V (после M3, и только если arch-слой изолирован)

- Требование к M1-M3: всё aarch64-специфичное — за trait (`arch::flush_tlb()`,
  `arch::page_table_entry()`, ...).
- Порт: второй `arch/riscv64/` + QEMU riscv64 virt; платы VisionFive 2 / Milk-V —
  те же driver-task'и.
- FOSS-полная история: MIT + открытая ISA + свой стек.

---

## M5. СООБЩЕСТВО (с момента M1.5)

- CONTRIBUTING.md: портирование = платформенное дерево (1 девайс = 1 файл + N
  driver-tasks) → чеклист-задача, ИИ-агент делает, человек ревьюит.
- Quality gate обязателен: фаззер + CI QEMU-boot для любого нового драйвера.
- «Reference device: Redmi 10C» в README — конкретная цель сообществу.
- TG t.me/rustdev_rs: регулярные срезы прогресса, каждый найденный фаззером баг =
  пост.
- Раз в месяц: peek чужих решений (Veda, Redox, pmOS) — заимствовать дизайн, не
  фичи.

---

## ПОРЯДОК РАБОТ (сводка)

```
Сейчас:  M1 (релиз 0.1.2 + демо + репо) — M0 закрыт 2026-11-15
Далее:   M2 (SECURITY.md → аудит точек входа → фаззер → lockdown) ‖ M3.1-M3.2
Потом:   M3.3 (Redmi bring-up, simplefb первой!) → M3.4 (GPU/freedreno)
Горизонт: M4 RISC-V, M5 сообщество-масштаб
```

## РОЛИ

- **DeepSeek/ИИ**: исполнение тасков, подготовка surfaces для аудита, шаблоны
  портов, тесты.
- **Аудитор (GLM)**: ревью точек входа ядра, классовая карта CVE, ревью
  policy-слоя, верификация исправлений, веб-проверка апстримов.
- **Владелец**: решения-развилки, код ядра каждый день, мерджи, релизы.
