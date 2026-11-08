# Вектор проекта — активный статус (2026-11-08)

Этот документ — источник истины о направлении. Технические детали — в
[`REFERENCE.md`](REFERENCE.md) и [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Решения

- **Планшет (Legion Y700 / SM8650) — забыт.** Профиль `tablet` остаётся в коде
  как исторический, но не входит в активную дорожную карту.
- **Активная платформа разработки — QEMU/десктоп.** Гость — AArch64 Silver,
  хост — Linux. Реальный телефон (Redmi 10C) — парковка до готовности
  OS-слоя.
- **Продукт — консольная WerewolfOS.** Shell, SFS, сеть, Unix-утилиты. Без
  композитора и домашнего экрана.
- **Браузер — отдельное окно, рендер на хосте.** Гость рисует в общий
  RGBA-буфер; Qt-окно на хосте показывает его и шлёт ввод. Композитор в госте
  не возвращается.
- **Linux ABI + Phosh — активная цель.** Горизонт — **месяцы** (решение
  владельца проекта). Ниже — узкий технический срез, который делает это
  достижимым.

## 1. Консоль (база)

Уже есть: Silver kernel, SFS, Scout, virtio-net + smoltcp (DHCP, ARP/IPv4,
ICMP, DNS, TCP), Howl, Howler, multi-call `utils` (`grep`, `wc`, `head`,
`tail`, `cp`, `find`, `seq`, `basename`, `dirname`, `true`, `false`,
`sleep`), сетевые команды `ping`, `ip`, `dns`, `wget` (HTTP в SFS) и
**Wolfser**: `wolfser` открывает поисковую строку; гость рисует страницу в
ivshmem-surface, Qt-viewer на хосте показывает кадр и шлёт клавиши, колесо и
клики через input ring. Есть кликабельные ссылки, история back/forward,
редиректы (3xx и meta refresh), перенос по словам, заголовки и reader-режим
(`<main>`/`<article>`). `http://` идёт через smoltcp, `https://` — через
TLS-прокси Qt-viewer’а в том же общем буфере.

Дальше: свой TLS-клиент, изображения (PNG), вкладки/закладки, `date` (RTC),
pager, больше applets.

## 2. Wolfser — окно браузера на хосте (первый срез)

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

## 4. Phosh (активная цель, месяцы)

Ключевой приём: **Phosh без GPU и DRM** на первом этапе.

- **Wayland-композитор**: `phoc`/`wlroots` с headless или pixman-бэкендом;
  композитный кадр уходит в общий буфер → host Qt-окно.
- **Рендер приложений**: GTK/Cairo в software-режиме; GPU/EGL не требуется
  для первого запуска.
- **D-Bus**: `dbus-broker` или совместимый демон; минимум для GNOME-сервисов.
- **systemd/logind**: заглушка-сервис с нужным D-Bus-интерфейсом
  (Phosh не требует полного systemd для базового запуска).
- **Ввод**: host input ring → Wayland seat (libinput-совместимый слой).
- **Сеть**: наш net_server + DHCP/DNS; NetworkManager можно заменить
  заглушкой на первом этапе.

Вехи:

| Веха | Результат |
| --- | --- |
| P1 | Linux ABI: busybox `sh` |
| P2 | headless Wayland-композитор рисует тестовое окно в host-окно |
| P3 | GTK3-приложение рисуется через software-рендер |
| P4 | D-Bus + заглушка logind, запуск `phosh` |
| P5 | Phosh shell + базовые приложения, ввод с хоста |

Риски (не отменяют цель): семантика D-Bus/systemd, динамический линкер и
glibc-поведение, корректность futex/потоков, производительность software-
рендера, ввод. Каждый риск закрывается своей вехой.

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
  QEMU/десктоп и Linux ABI/Phosh.
- **Своя графика/композитор**: удалены; окно — только host Qt + один surface.

## 6. Ближайший порядок работ

1. Сеть: TLS-клиент и `https://` (rustls), затем HTTP/1.1 keep-alive.
2. Wolfser: редиректы, изображения/ссылки, damage, больше ввода; затем Linux ABI.
3. Linux ABI: статические musl-бинарники, затем busybox `sh`.
4. Phosh: headless Wayland → GTK → D-Bus → `phosh`.
5. По желанию: свой минимальный браузер в host-окне (или QtWebEngine как
   демо).
