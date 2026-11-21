# Вектор проекта — активный статус (2026-11-20)

Этот документ — источник истины о направлении. Технические детали — в
[`REFERENCE.md`](REFERENCE.md) и [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Решения

- **Планшет (Legion Y700 / SM8650) — забыт.** Профиль `tablet` остаётся в коде
  как исторический, но не входит в активную дорожную карту.
- **Активная платформа разработки — QEMU/десктоп.** Гость — AArch64 Silver,
  хост — Linux. Реальный телефон (Redmi 10C) — парковка до готовности
  OS-слоя.
- **Продукт — консольная WerewolfOS как база.** Ядро, драйверы, сеть, файлы
  и базовые операции остаются нашими. Прикладной слой (браузер, GUI и
  остальные программы) приходит **из Linux** через слой совместимости, а не
  пишется заново в нативной userspace. Без композитора и домашнего экрана в
  базовой ОС.
- **Wolfser — временный браузер (stopgap), не продукт.** Он остаётся, пока
  Linux-стек не поднимет настоящий движок, и не развивается как продуктовый
  браузер. Свои приложения в нативной userspace не наращиваем.
- **Сеть — наша, в консоли.** `net_server`/smoltcp (DHCP, DNS, TCP, HTTP,
  TLS) остаётся базовой возможностью ОС; Linux-приложения получают её через
  слой совместимости.
- **Linux ABI + Denial DE — активная цель** (решение 2026-11-19: phosh
  брошен, прикладной DE — Denial/denialwm, Smithay+Flutter). Горизонт —
  **месяцы**. Ниже — узкий технический срез, который делает это достижимым.

## 1. Консоль (база)

Уже есть: Silver kernel, SFS, Scout, virtio-net + smoltcp (DHCP, ARP/IPv4,
ICMP, DNS, TCP), Howl, Howler, multi-call `utils` (`grep`, `wc`, `head`,
`tail`, `cp`, `find`, `seq`, `basename`, `dirname`, `true`, `false`,
`sleep`), сетевые команды `ping`, `ip`, `dns`, `wget` (HTTP в SFS) и
**Wolfser**: `wolfser` открывает поисковую строку; гость рисует страницу в
ivshmem-surface, Qt-viewer на хосте показывает кадр и шлёт клавиши, колесо и
клики через input ring. Есть кликабельные ссылки, история back/forward,
редиректы (3xx и meta refresh), перенос по словам, заголовки и reader-режим
(`<main>`/`<article>`). Четыре вкладки хранят URL, заголовок, прокрутку,
историю и кэш отрендеренной страницы, поэтому переключение мгновенное.
Наведение на ссылку показывает адрес внизу и включает курсор-руку; средний
клик или Ctrl+клик открывает её в новой вкладке. Основной рендерер —
**litehtml** (BSD-3): настоящая блочная/инлайновая раскладка CSS, поля, цвета,
списки, рамки и hit-testing ссылок; страницы Wikipedia открываются в
reader-режиме по `mw-content-text`/`main`/`article`, остальные — как есть с
inline-CSS и внешними стилями до 64 KiB. Закладки `Ctrl+D` сохраняются в
`/sfs/wolfser.bookmarks`. Картинки `<img>` гость скачивает через net_server и
декодирует сам через `stb_image` (PNG/JPEG/GIF): до четырёх на страницу,
512 KiB в сжатом виде и 300k пикселей на кадр, RGBA-кэш рисуется
масштабированно. Старый текстовый рендерер с картинками остаётся запасным.
`Ctrl+F`
ищет по тексту страницы, подсвечивает все совпадения (текущее — оранжевым) и
переходит по Enter. Новая вкладка, кнопка `H` или `Alt+Home` открывают
стартовую страницу с быстрыми ссылками и постоянными закладками. Шесть
вкладок, зум окна `Ctrl++`/`Ctrl+-`/`Ctrl+0`. `http://` и `https://` (TLS 1.3
в госте) идут через smoltcp; Qt-прокси остаётся запасным путём и декодером
картинок.

Формы: `Ctrl+G` открывает первое текстовое поле страницы в футере, Enter
отправляет GET/POST (скрытые поля и сессионные cookies включаются). Текст
рисует жирный и нумерованные списки; загрузка сохраняет старую страницу с
спиннером, а хост копирует только завершённый кадр. Это всё ещё текстовый
reader: DOM/JavaScript/CSS-движка и медиа-конвейера нет, поэтому **YouTube
здесь работать не может** — для него нужен WebKit/Gecko-класс движок по
треку Linux ABI + Phosh. Дальше: укрепление TLS (больше корней, RTC,
энтропия, TLS 1.2), полноценные формы/виджеты, keep-alive,
damage-прямоугольники, `date` (RTC), pager, больше applets.

**Статус Wolfser (решение 2026-11-09):** это временный браузер и демонстрация
окна/рендера, **не продуктовый путь**. Приложения, включая браузер, приходят
из Linux через слой совместимости; нативную userspace-обвязку не наращиваем.
Wolfser поддерживается только чтобы было чем смотреть страницы, пока
Linux-стек не готов.

## 2. Wolfser — временное окно браузера (stopgap)

```
гость (browser)                          хост (Qt)
+----------------------+   shared mem   +----------------------+
| RGBA surface         | <------------> | werewolf-window      |
| frame_seq + input    |                | blit + key/wheel     |
+----------------------+                +----------------------+
```

- Общий буфер: `ivshmem-plain` на QEMU (`memory-backend-file`); BAR2
  ассайнится гостем и мапится как Normal Non-Cacheable.
- Протокол: заголовок (magic/width/height/stride/format/frame_seq/state),
  input ring (64 записи: key/scroll/click), пиксели с `0x1000`.
- Гость: `wolfser` — поисковая строка/адрес, HTTP через `net_server`,
  HTML → текст, отрисовка 5x7-шрифтом, Ctrl+L, скролл/перезагрузка/выход.
- Хост: `tools/window-viewer` (Qt6), плюс `--snapshot` для headless-проверки.
- Никакого DRM/GPU в госте: один surface, программный рендер.
- Поиск по умолчанию — DuckDuckGo HTML over HTTPS (через хост-прокси).
  Следующее: изображения, вкладки/закладки, damage-прямоугольники, свой TLS.

Браузерная логика — в госте: выбор backend’а, HTML-токенизатор, ссылки,
история, раскладка и редиректы. HTTPS на первом этапе — host-proxy (хост
забирает URL по TLS и отдаёт документ в общий буфер), позже — свой TLS-стек.
QtWebEngine на хосте остаётся быстрым демо-путём, но не продуктом.

## 3. Linux ABI

Personality-слой в ядре: у задачи флаг `Abi::Linux`; на `svc` ядро разбирает
Linux-номер syscall (x8) и транслирует в VFS/память/IPC.

Этапы:

1. **Статические musl-бинарники**: `hello`, отдельные applets busybox.
   Syscalls: `read/write/openat/close/lseek/fstat/mmap/munmap/brk/exit/
   exit_group/clock_gettime/getpid/getuid/getrandom/ioctl(TTY)/poll`.
2. **Потоки и синхронизация**: `clone` (thread), `futex`, TLS, `rt_sigaction`,
   `rt_sigprocmask`, `tgkill`.
3. **Динамический линкер**: `ld-musl-aarch64.so.1`, `PT_INTERP`, `DT_NEEDED`,
   `mprotect`, `madvise`, `prctl`.
4. **Совместимость окружения**: `/proc`, `/sys`, `/dev/null`, `/dev/urandom`,
   `epoll`, `eventfd`, `memfd`, `socketpair`/`AF_UNIX` (минимум для D-Bus).

Acceptance: busybox `ls/cat/sh` и статический `hello` работают без
Silver-специфичных патчей.

## 4. Denial DE (активная цель, месяцы)

**Пивот 2026-11-19:** phosh/phoc брошены (upstream-баги, GTK-стек тяжёлый).
Прикладной DE — **Denial (denialwm/denial)**: Wayland-композитор на Smithay,
шелл — Flutter, встроенный в процесс композитора (deniald). Скриншот-путь
не нужен: рендер идёт через наш DRM (virtio-gpu) → scanout.

- **Стек**: Alpine edge boot chain + Debian trixie arm64 glibc-рунтайм
  (mesa/llvmpipe, seatd, libinput, glib) + denial aarch64 пакеты;
  образ `build/werewolf-alpine-glibc-denial.img`, билдер
  `tools/build_glibc_denial.py`, резолвер `tools/debian_repo.py`.
- **deniald** запускается с `--flutter-bundle /usr/lib/denial/flutter
  --wayland --no-xwayland` (без бандла — RuntimeLimit::TestOnly, выход после
  self-теста).
- Ядро получило большой glibc-компат-слой: signalfd, inotify, fsync,
  rt_sigtimedwait, SOCK_SEQPACKET, AF_NETLINK-стаб (udev monitor),
  sched_*/priority, procfs maps/environ/cpuinfo, mkdirat-ENOENT семантика,
  socket-namespace stat/unlink, group-kill при терминирующих сигналах,
  полный DRM atomic UAPI (SET_CLIENT_CAP, planes, properties, blobs,
  CLOSEFB, PRIME export/import, ADDFB2).
- **Текущий статус**: deniald доходит до "Created new socket wayland-1"
  (Wayland-сервер) и "DrmDevice initializing"; текущий стоп — assert
  systemd-crate `isatty_safe(fd=13)` → EBADF (fd не открыт у нас). Детали и
  методики — в `handoff.md` "2026-11-20".

Вехи:

| Веха | Результат |
| --- | --- |
| P1 | Linux ABI: busybox `sh`, glibc-бинарники |
| P2 | deniald проходит seatd/DrmDevice/EGL (llvmpipe) |
| P3 | [сделано] deniald: Wayland-сокет, portal IPC, GPU transfer, atomic TEST_ONLY, control socket — полный DE-флоу до старта Flutter |
| P4 | [текущая точка] Flutter engine стартует; стоп = ICU init на icudtl.dat (см. handoff "2026-11-21"); затем render loop + шелл на экране |
| P5 | screencopy-minimal как первый внешний Wayland-клиент |

Риски: glibc-поведение (треды/exit_group/futex), системная интеграция deniald
(systemd-crate, portal IPC, D-Bus), ввод через udev/evdev, производительность
llvmpipe. Каждый риск закрывается своей вехой; диагностика — кольца + pmemsave.

## 4a. Референс: Veda (только архитектура)

Публичный проект [Veda](https://github.com/vahmoh/Veda) — независимое
подтверждение выбранного вектора: Rust-microkernel, сервисы в userspace,
сеть отдельным сервисом на smoltcp, POSIX-слой, Linux-совместимость и
браузер на портированном WebKit. Мы **не копируем** его код (GPL-3.0) и не
берём ассеты; полезны только архитектурные решения:

- ядро не знает про сеть: один сервис `network` поверх smoltcp;
- DHCP/DNS/TCP — в этом сервисе; клиенты ходят через общий буфер/ring;
- TLS — rustls + webpki-roots (no_std), HTTP — свой клиент поверх TCP;
- браузер — отдельный процесс; у них это порт WebKit (месяцы работы), у нас
  ближайший шаг — свой HTTP + HTML-подмножество в host-окно, WebKit/Phosh —
  следующий трек;
- POSIX-слой, затем Linux ABI — тот же порядок, что в нашем плане.

Их `NETWORKING.md` и `BROWSER.md` — хорошая карта рисков (TLS, sandbox,
потоки, D-Bus); реализация остаётся нашей.

## 5. Что паркуется

- **Legion Y700 / SM8650**: профиль `tablet` собирается, но не развивается.
- **Redmi 10C / fog**: RAM-only порт остаётся задокументированным
  ([`PORTING_FOG.md`](PORTING_FOG.md)), но активная работа — сначала
  QEMU/десктоп и Linux ABI/Denial.
- **Phosh/phoc**: брошены 2026-11-19 (upstream phoc 0.55.1 heap corruption
  и тяжёлый GTK-стек); образы и наработки сохранены, не развиваются.
- **Своя графика/композитор**: удалены; окно — только host Qt + один surface.

## 6. Ближайший порядок работ

1. **Denial DE** — единственный путь к браузеру и приложениям:
   - [сделано] Linux ABI (статика+динамика glibc), файлы, треды, futex,
     Wayland/D-Bus транспорт, полный DRM atomic, glibc-компат-слой;
   - [сделано] deniald проходит seatd → DrmDevice → EGL → Wayland-сокет;
   - дальше: assert `isatty_safe(fd=13)` (текущий стоп, см. handoff.md);
   - затем: render loop/Flutter-шелл, evdev/libinput ввод;
   - затем: screencopy-minimal как первый внешний клиент.
2. Сеть как база ОС: keep-alive, больше корней TLS, RTC/энтропия. Это
   обслуживание базы, не прикладной слой.
3. Wolfser: только поддержка current-состояния (багфиксы), без новых
   функций. Собственный минимальный браузер в host-окне — не развиваем.
4. Парковка: планшет, Redmi, phosh, своя графика/композитор.
