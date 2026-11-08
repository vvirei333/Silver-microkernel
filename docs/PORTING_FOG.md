# Porting the console OS to the Redmi 10C (`fog`)

This is the working plan between "kernel boots on the phone" and "the phone
runs the console OS". The authoritative boot/relocation record remains
[`redmi10c_poc/README.md`](../redmi10c_poc/README.md).

The graphics subsystem (compositor, framebuffer backends, kernel surfaces,
input driver, home screen) was **removed on 2026-11-08** after reconsidering
the project's development vector. The phone port is therefore a console port:
kernel + SFS + nameserver + Howl + UART. A single browser window is a later,
separate deliverable with its own minimal display path.

## Rules for hardware work

- **No flash, no unlock, no EDL, no destructive probe.** The only supported
  device action is RAM-only `fastboot boot`.
- One hardware variable per boot. Record the image commit, DTB hash and serial
  log (or the panel state when UART is unavailable).
- Every address comes from the live FDT or a documented bootloader contract.

## Current seams

- **FDT probe**: model/compatible, `/memory`, `/reserved-memory`, GICv3 bases,
  timer INTID and UART base all come from the live tree.
- **Relocation**: standard fog enters at the link address and parks on a
  mismatch; `fog-reloc` performs the checked copy after live-FDT validation.
- **Console**: GENI UART TX now fails fast after 500k spins and stops after the
  first timeout, so an unclocked UART no longer stalls the boot. UART
  clock/pins are still unverified.
- **Kernel SFS**: `virtio-blk` is QEMU-only by design; the phone build skips
  the PCI probe (`sfs::load()` reports no device). Phone storage needs an
  eMMC/UFS driver that satisfies the same block contract.
- **Power**: PSCI through SMC (Qualcomm conduit). `fog-auto-reset` resets a
  RAM-only diagnostic image after 30 s; verified on `rain`.

## Staged plan

| Stage | Goal | State |
| --- | --- | --- |
| 0 | Boot stub, FDT proof, MMU, GICv3, timer, scheduler, Howl | done on `rain` |
| 1 | Console OS over UART on the phone | blocked on UART clock/pins |
| 2 | Persistent storage (eMMC/UFS) for `/sfs` | not started |
| 3 | Battery/thermal/power key, safe shutdown | not started |
| 4 | SMP and network bring-up on the phone | not started |
| 5 | Minimal window + basic browser (separate deliverable) | out of scope for this port |

## Real-device findings (2026-11-07, `rain`)

1. **Kernel SFS probed QEMU PCI on the phone.** `virtio_blk::init()` scanned
   ECAM `0x40_1000_0000`; the first config read faulted and the unit stopped at
   the frame-allocator stage. The probe is now QEMU-only.
2. **GENI UART TX could stall for ~100 ms per byte.** The poll budget was
   50,000,000 spins. It is now 500,000 with a sticky `TX_STALLED` flag.
3. **The newer MMU boot proofs wedge the unit.** The first TTBR0 TLB self-test
   hangs on `rain`; the default fog image skips the post-DMA-window proofs and
   logs `SILVER DEEP SELFTESTS SKIPPED`. `fog-hw-selftests` opts back in.
4. **PSCI `SYSTEM_RESET` over `smc` works.** `hvc` is not valid there. The
   opt-in `fog-auto-reset` returns the unit to Android without a power-key hold.
5. **Graphics removed.** The `fog-display`/`fog-ui` paths were deleted with the
   rest of the graphics subsystem; they are no longer part of this port.

## Hardware unknowns

- UART pins/clock and whether the GENI console can be read on this unit;
- storage controller (eMMC vs UFS) and the real partition layout;
- PSCI conduit confirmation beyond the reset test, and power-key wiring;
- thermal/battery registers (SPMI/PMIC).

## Regression matrix

```bash
cargo test --lib
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-reloc
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-auto-reset
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,tablet
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,smp-selftest
```

QEMU:

- normal boot: console OS, SFS mounted, `howl ready`, `help`/`ls`/`health`;
- fog baseline: synthetic khaje DTB, `-m 128M`, `howl ready`;
- fog-reloc: relocation and fail-closed cases from `test_fog_qemu.sh`.
