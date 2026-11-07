# Porting WerewolfOS to the Redmi 10C (`fog`)

This is the working plan between "kernel boots on the phone" and "the phone
runs the actual OS". It is a checklist and a map of code seams, not a claim
that the hardware port is finished. The authoritative boot/relocation record
remains [`redmi10c_poc/README.md`](../redmi10c_poc/README.md).

## Rules for hardware work

- **No flash, no unlock, no EDL, no destructive probe.** The only supported
  device action is RAM-only `fastboot boot` of a built image.
- One hardware variable per boot. Record the image commit, DTB hash and serial
  log (or the panel state when UART is unavailable).
- Every address comes from the live FDT or a documented bootloader contract.
  A hardcoded address is a bug unless it is a last-resort default with a
  fail-closed check.
- If a device path cannot be proven, park and paint a visible failure state.
  Never continue with an unverified memory map or clock.

## Current seams (already in the tree)

### Platform framebuffer (`fog-display`)

The bootloader configures the panel and leaves `cont_splash` in
`/reserved-memory`. The kernel can now:

1. find the carve-out by name (`cont_splash*` or `splash*`) in the live FDT,
2. prove it lies inside a `/memory` region and is large enough for the panel
   mode, and register it as the platform display (`src/display.rs`),
3. hand it to userspace with `SYS_DISPLAY_INFO` and `SYS_DISPLAY_MAP`
   (`CAP_SURFACE` required; the physical range is fixed and cannot be
   re-pointed).

The userspace compositor (`gpu_driver`) probes `SYS_DISPLAY_INFO` at startup:

- **available:** maps the panel buffer, uses it as `FRAMEBUFFER`, skips
  `fw_cfg`/`ramfb`, and refuses `GUI_SURFACE_ATTACH` (the panel scans one fixed
  range);
- **unavailable:** keeps the QEMU `ramfb` backend unchanged.

SilverHome sees the platform display and skips zero-copy surfaces, so it uses
the command-stream path into the compositor's framebuffer. This is not a
regression of the QEMU zero-copy path; it is the correct fallback when there is
no device to re-point.

Build (experimental, real-device use only):

```bash
cargo build --release --target aarch64-unknown-none \
  --no-default-features --features no_std,fog,fog-reloc,fog-display
```

`fog-display` is deliberately **not** combined with `fog-ui`/`fog-diag`: all
three own the same `cont_splash` buffer. The QEMU fog test uses `-m 128M` and a
synthetic tree whose memory is smaller than `0x5C00_0000`; enabling
`fog-display` there would map address space QEMU does not back. Use it only
with a tree and a `-m` size that really contain the splash carve-out.

### Verified in QEMU with a 1 GiB synthetic khaje tree

- `SILVER DISPLAY OK base=0x5C000000 size=0xF00000 mode=720x1650`
- compositor maps the panel through `SYS_DISPLAY_MAP`
- SilverHome's pixels appear at `0x5C000000` (`0xFF194A87` wallpaper,
  `0xFF235FA7` icon, `0xFF1C1C1E` dark icon)
- `howl ready`, no panic
- default `fog` (without `fog-display`) still boots with no display line
- normal QEMU still uses `ramfb` + zero-copy surfaces

The real-device boot of `fog-display` remains a hardware check.

## Staged port plan

| Stage | Goal | State |
| --- | --- | --- |
| 0 | Boot stub, FDT proof, MMU, GICv3, timer, scheduler, Howl | done on `rain` |
| 1 | Panel framebuffer + real compositor/SilverHome | hardware: maps/clears; HOME draw still pending; QEMU-verified at 720x1650 |
| 2 | Touch input (GENI I2C + touch controller) | not started |
| 3 | Persistent storage (eMMC/UFS) for `/sfs` | not started |
| 4 | Battery/thermal/power key, safe shutdown | not started |
| 5 | Native DSI/GPU handoff (stop relying on `cont_splash`) | not started |
| 6 | Wi-Fi, audio, modem, camera | out of scope for the first port |

### Stage 2 — touch

Needed before the UI is usable:

- enumerate the touch controller from the live FDT (`/soc/...i2c...` child,
  `compatible` string, interrupt line);
- add a GENI I2C driver (polled first; interrupts later) and a controller
  driver for the part actually populated on the unit;
- produce the same `InputMsg` stream the QEMU `input_driver` produces, so the
  compositor does not change;
- calibrate the reported range from the controller/FDT, not QEMU's `0..32767`;
- fail closed: if no touch device is proven, the compositor still renders and
  the shell stays usable over UART.

### Stage 3 — storage

The kernel currently owns only virtio-blk. A phone port needs an eMMC/UFS
driver that satisfies the same SFS contract: sector reads/writes, device
capacity, cache maintenance and a proven quiescence point before abandoning
the bootloader's storage state. Partition discovery must come from the FDT
or GPT, never a hardcoded offset.

### Stage 4 — power and thermal

- PSCI on Qualcomm uses `smc`, not the QEMU `hvc` path (`sys_power_off` must
  select per platform).
- Read battery/thermal from the PMIC (SPMI) and refuse to start heavy work on
  an unproven thermal state.
- Power-key handling belongs in a real input driver, not the QEMU menu.

### Stage 5 — display ownership

`cont_splash` is a bring-up contract, not a display driver. The real port
needs an MSM DSI/DPU driver: panel mode from the FDT or a documented panel
database, clock/regulator setup, and a clean handoff from the bootloader
framebuffer. Until then, the platform framebuffer path is the safe fallback.

## Hardware unknowns to confirm on the unit

- exact panel mode, stride and color order (the current assumption is
  720x1650, stride 720, ARGB8888);
- whether `cont_splash` remains continuously scanned after ABL hands off on
  this firmware revision (the `fog-ui`/`fog-display` panel is the test);
- whether the splash carve-out is cacheable/dirty when we take it over (the
  kernel invalidates the range before mapping);
- touch controller part number, I2C bus and interrupt;
- storage controller (eMMC vs UFS) and the real partition layout;
- PSCI conduit and reset/power-key wiring;
- UART pins/clock (console is still unverified on `rain`).

## Regression matrix

Host:

```bash
cargo test --lib                                  # 218 tests
```

Builds that must stay green:

```bash
# default QEMU + zero-copy
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std

# fog baseline (no platform framebuffer, no compositor spawn)
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog

# fog with the experimental panel UI
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-reloc,fog-display

# other required profiles
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-ui
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,tablet
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,smp-selftest
```

QEMU:

- normal boot: `SILVER` home frame, zero-copy `GUI_SURFACE_ATTACH`, click into
  Calc and back;
- fog baseline: synthetic khaje DTB, `-m 128M`, no `SILVER DISPLAY` line,
  `howl ready`;
- fog-display: synthetic tree with `cont_splash` and `-m 1G`,
  `SILVER DISPLAY OK`, non-black pixels at `0x5C000000`, `howl ready`.

Collect the exact commands and logs in the bring-up notes; a screenshot of the
panel is the primary evidence when UART is unavailable.

## Real-device findings (2026-11-07, `rain`)

RAM-only `fastboot boot` of the `fog-display` image exposed six issues that
QEMU cannot show. Four are fixed in the tree, one is the next task, and the
last is still open:

1. **Kernel SFS probed QEMU PCI on the phone.** `sfs::load()` called
   `virtio_blk::init()`, which scanned the ECAM window at `0x40_1000_0000`.
   That address is not mapped on fog, so the first config read took a
   synchronous fault and the unit stopped at the "frame allocator OK" stage.
   `virtio_blk::init()` now returns `false` unless the platform is `Qemu`;
   storage on a phone must come from the future eMMC/UFS driver.
2. **GENI UART TX could stall for ~100 ms per byte.** `poll_status` spun up to
   50,000,000 times before aborting. The unit's UART is not proven clocked, so
   every console line delayed boot by seconds and the scheduler started much
   later than the diagnostic deadlines. The budget is now 500,000 spins and
   the first timeout sets `TX_STALLED`, after which TX returns immediately.
3. **The newer MMU boot proofs wedge the unit.** The first TTBR0 TLB self-test
   (added after the last hardware run) hangs on `rain`. A default fog image now
   skips the post-DMA-window MMU/ASID/heap/DMA proofs and logs
   `SILVER DEEP SELFTESTS SKIPPED (fog hardware profile)`. The
   `fog-hw-selftests` feature opts back in for a dedicated debugging image.
   This is a tactical skip: the proofs must be debugged before the port claims
   MMU/TLB coverage on hardware.
4. **PSCI `SYSTEM_RESET` over `smc` works on the unit.** `hvc` is not a valid
   conduit there. The opt-in `fog-auto-reset` feature resets the SoC 30 s after
   the timer is up, so a RAM-only diagnostic image returns to Android without a
   power-key hold. This is diagnostic-only and is not part of a release image.
5. **The platform panel path maps and clears.** With `fog-display`, userspace
   maps the live `cont_splash` carve-out through `SYS_DISPLAY_MAP` and wipes
   the panel, so the bootloader splash disappears on the real unit.
6. **HOME/compositor use the live panel geometry.** `SYS_DISPLAY_INFO` now
   supplies 720x1650 to SilverHome, and the platform path draws directly into
   the fixed scanout buffer. The old 640x1136 offscreen surface is QEMU-only:
   creating it on the phone made `present_surface()` reject the blit (size
   mismatch) and left the panel black after the clear.

**Still open:** after the compositor's initial clear the panel stays black on
`rain`; SilverHome's command stream is not reaching the panel yet. QEMU with a
1 GiB synthetic khaje tree draws the full 720x1650 UI (wallpaper pixels at
`0x5C0E1000`), so the remaining fault is in the hardware handshake
(name registration, UI ownership or the first `GUI_RECT`). Instrument that
handshake before touching touch/storage.

The image name for the current bring-up check is
`redmi10c_poc/out/boot-reloc-display-reset.img` (features
`no_std,fog,fog-reloc,fog-display,fog-auto-reset`). `build_boot_image.sh`
accepts `--display`, `--paint` and `--autoreset`.
