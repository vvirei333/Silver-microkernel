# Как устроен postmarketOS/Nura как мобильный Linux

Дата: 2026-11-15. postmarketOS в сентябре 2026 переименован в **Nura**;
здесь сохранено привычное имя. Актуальный релиз — v26.06 «Alpen Avocado»:
Alpine 3.24, systemd 261 (systemd-вариант), GNOME 50, Phosh 0.55,
greetd + phrog вместо tinydm. Источники: release notes v26.06, state/Nura,
pmaports, deviceinfo reference.

Это не обзор «как собрать pmOS», а карта слоёв и границ ответственности,
полезная для решений WerewolfOS.

## 1. Слои Mobile Linux

```text
┌─────────────────────────────────────────────────────────────┐
│ приложения: GTK/GNOME, Phosh-плагины, Flatpak/APK-пакеты    │
├─────────────────────────────────────────────────────────────┤
│ сессия: greetd → phrog/phosh-session → phoc → Phosh         │
│ сервисы: D-Bus, polkit, NetworkManager, UPower/logind,      │
│          ModemManager, BlueZ, PipeWire, iio-sensor-proxy    │
├─────────────────────────────────────────────────────────────┤
│ rootfs: Alpine (apk), OpenRC или systemd, /etc, /usr        │
├─────────────────────────────────────────────────────────────┤
│ initramfs: модули, unlock, resize, mount root, switch_root  │
├─────────────────────────────────────────────────────────────┤
│ kernel: mainline Linux + Device Tree (+ модули/firmware)    │
├─────────────────────────────────────────────────────────────┤
│ boot: Android boot image / lk2nd / U-Boot / boot-deploy     │
│ deviceinfo: kernel, dtb, cmdline, partitions, flash method  │
└─────────────────────────────────────────────────────────────┘
```

Ключевая идея pmOS: **не тащить Android userspace и Android kernel**, а
взять обычный Linux-стек и описать конкретный телефон набором пакетов и
`deviceinfo`. Отсюда 500+ портов и 254 устройства в testing на v26.06.

## 2. Boot и портирование

- **`pmbootstrap`** — хост-инструмент: создаёт chroot, собирает ядро и
  пакеты, генерирует rootfs и install/flash-образ. Один телефон = один
  набор `device-*` пакетов в `pmaports`.
- **`deviceinfo`** — декларативный манифест порта: `deviceinfo_kernel`,
  `deviceinfo_dtb`, `deviceinfo_cmdline`, `deviceinfo_flash_method`,
  `deviceinfo_partitions_*`, `deviceinfo_screen_width/height` и т.д.
  `boot-deploy` превращает kernel+dtb+initramfs в образ, который понимает
  загрузчик устройства.
- **Загрузчик**: часто Android boot image (fastboot), для старых устройств —
  lk2nd/U-Boot; `boot-deploy` умеет оба мира.
- **Ядро**: mainline-первый подход; для части устройств — downstream Android
  kernel. Device Tree описывает панель, тач, регуляторы, UART, USB, modem,
  камеры, звук.
- **initramfs**: `postmarketos-initramfs`/mkinitfs; внутри — модули для
  rootfs (ext4/f2fs/USB/display), разблокировка (osk-sdl), resize, mount,
  `switch_root`. Это отдельный маленький Linux, а не часть rootfs.
- **rootfs**: Alpine Linux, `apk`; `/usr` — пакеты, `/etc` — конфигурация;
  обновление — `apk upgrade`, без A/B-слотов (риск: питание при обновлении).
- **Категории устройств**: main/community/testing/archived; качество
  поддержки — это социальный контракт, а не только код.

## 3. Runtime: сессия и сервисы

- **init**: OpenRC (исторический default) или systemd (вариант для
  GNOME/Plasma; v26.06 — systemd 261). Юниты задают зависимости, restart,
  cgroups, logind.
- **display manager**: v26.06 перешёл с tinydm на **greetd + phrog** —
  upstream-рекомендация Phosh.
- **композитор**: **phoc** (wlroots) — Wayland-композитор, рисует через
  pixman/GL, владеет DRM/KMS и вводом.
- **shell**: **Phosh** (GTK3, GNOME mobile) — lock screen, panel, app grid,
  quick settings, OSK, плагины. Phosh — клиент phoc, а не отдельная ОС.
- **сервисы, без которых Phosh деградирует**:
  - D-Bus + polkit — авторизация, агенты, OSK, session manager;
  - NetworkManager — Wi-Fi/WWAN, Phosh wifi/quick settings;
  - UPower + logind — батарея, яркость, suspend, lock-delay;
  - ModemManager — звонки/SMS/данные;
  - BlueZ — Bluetooth;
  - PipeWire + WirePlumber — звук;
  - iio-sensor-proxy — авто-поворот;
  - libcamera — камеры;
  - Plymouth — boot splash;
  - osk — экранная клавиатура (Phosh OSK manager).
- **GPU**: Mesa/freedreno (Qualcomm), panfrost (ARM), etnaviv и т.д.;
  без GPU Phosh работает через pixman, но с высокой нагрузкой на CPU.

## 4. Модель безопасности

pmOS/Nura — обычный Linux: root, users/groups, capabilities, seccomp,
namespaces, `sudo-rs`, подписанные пакеты Alpine. По умолчанию нет SELinux/
AppArmor как обязательного барьера. Драйверы — в kernel space; любая ошибка
в драйвере/ядре — потенциальный root. Это противоположность микроядерной
модели Silver: маленькое ядро, драйверы в EL0, capability-таблицы, IPC.

## 5. Что из этого уже есть в WerewolfOS

Текущий Phosh-путь — это **настоящий Phosh на Linux ABI**, но не полный
pmOS-сеанс:

| Слой pmOS | WerewolfOS сейчас |
|---|---|
| bootloader/deviceinfo | `platform::detect()` + встроенный FDT/`linker*.ld`; манифеста устройства для Redmi нет |
| Linux kernel + DT | Silver microkernel; драйверы EL0 (virtio, UART), Linux-драйверы не переиспользуются |
| initramfs | нет; ядро монтирует SFS и сразу стартует SilverD |
| Alpine/apk | SFS-образ, `seed.rs`/bundle manifests; пакетного менеджера и обновлений нет |
| OpenRC/systemd | SilverD со статическим порядком spawn; супервизора/restart нет |
| greetd/phrog | `linuxstart phosh` → `/sfs/phosh-serve` → `/sfs/phosh`; PAM/logind нет |
| phoc | `phosh-serve` — свой Wayland-сервер (software, ivshmem); phoc-путь существует отдельно |
| D-Bus | свой dbus-daemon с permissive config; часть сервисов не поднимается |
| NetworkManager | native `net_server`/smoltcp; Phosh wifi-manager получает connection refused |
| UPower/logind | нет; battery/suspend/screen-saver менеджеры Phosh логируют ошибки |
| ModemManager/BlueZ/PipeWire/iio/libcamera | нет |
| обновления | перезапись SFS-образа; A/B и atomic update нет |

Поэтому лог Phosh полон `Could not connect: Connection refused` — это не
баги Phosh, а отсутствующие сервисы. Сам shell, lock screen, app grid,
Terminal и ввод работают.

## 6. Что стоит перенести в архитектуру WerewolfOS

1. **Device manifest для Redmi 10C.** Аналог `deviceinfo`: kernel/DT, панель,
   тач, UART, регуляторы, USB, modem, partitions, boot method. Один файл,
   который читают загрузчик и драйверы, вместо условий в `platform.rs`.
2. **Супервизор сервисов.** Не статический список spawn, а граф
   зависимостей, restart policy, capability-гранты на старте, логи. Это
   заменит и OpenRC/systemd, и текущий порядок SilverD.
3. **Session manager.** Аналог greetd/phrog: запуск композитора, Phosh,
   D-Bus, polkit, обработка logout/lock. Сейчас `linuxstart phosh` — ручной
   путь.
4. **Мосты к Linux-сервисам.** Либо native-реализации (net_server уже есть),
   либо Linux ABI-сервисы: NetworkManager поверх netlink/ioctl, UPower/
   logind стабы, PipeWire. Пока Phosh видит только D-Bus и сеть.
5. **Power/thermal/suspend.** Для телефона это не «фича», а выживание:
   cpufreq, thermal zones, charging, suspend/resume, RTC wake.
6. **Дисплей и ввод.** На QEMU — software scanout; на Redmi — simplefb,
   затем DRM/KMS + touch; phoc можно запускать поверх DRM-бэкенда, если
   появятся KMS/evdev-интерфейсы, иначе останется свой compositor.
7. **Обновления.** SFS-бандлы уже подписаны/квотированы; следующий шаг —
   atomic generation/rollback (лучше, чем apk без A/B).
8. **Безопасность.** Не копировать Linux-root модель. Linux ABI-таски
   сейчас все uid 0; для реального multi-user нужен маппинг uid↔capability
   и запрет ambient authority.

## 7. Чего не делать

- Не тащить весь Alpine rootfs и systemd в микроядро «как есть»: это
  потребует Linux-драйверов, netlink, udev, cgroups, DRM ioctls — то есть
  фактически второго Linux.
- Не начинать с GPU. pmOS сам годами живёт на pixman/llvmpipe; для первой
  картинки на Redmi достаточно simplefb.
- Не смешивать driver-модель pmOS (kernel modules) с EL0-драйверами Silver
  без явного контракта: каждый драйвер должен получить MMIO/IRQ/DMA через
  capability, а не через `/dev` и root.

## 8. Практический вывод для M3

Порядок, который следует из этой карты:

1. Redmi: device manifest + UART + simplefb (первая картинка).
2. Супервизор + session manager: убрать ручной `linuxstart`, поднять
   logind/UPower-стабы, чтобы Phosh не деградировал.
3. Сеть: оставить native net_server, но дать Phosh/NetworkManager мост
   (или заменить NM на native-провайдер).
4. DRM/KMS + touch + power/thermal.
5. Только потом GPU/камеры/модем и пакетный/атомарный апдейт.

Phosh на Linux ABI — правильный «верх»; ближайшая работа — дать ему
нормальный мобильный «низ» и сервисы, а не переписывать shell.
