# WerewolfOS reference console — acceptance and status

The product is a small, solid **console OS**:

```text
Silver kernel
  + kernel SFS (/sfs, read-write)
  + nameserver (Scout)
  + network driver + smoltcp server
  + Howl shell + coreutils
  + Howler UART input
```

No compositor, no framebuffer backend, no input tablet driver, no home screen
and no kernel surface objects. The graphics subsystem was removed on
2026-11-08 after reconsidering the project's development vector: a full
graphical OS is outside the scope that can be finished well here. A single
window will return later only as a host for a basic browser, with its own
minimal display path; it is not a desktop.

## Build and run

```bash
# product image (console)
cargo build --release --target aarch64-unknown-none \
  --no-default-features --features no_std

# QEMU smoke test
qemu-system-aarch64 -machine virt,virtualization=on -cpu cortex-a72 -smp 1 \
  -m 256M -kernel target/aarch64-unknown-none/release/silver_microkernel \
  -drive file=<sfs.img>,format=raw,if=none,id=hd \
  -device virtio-blk-pci,drive=hd -display none -serial stdio
```

`virtualization=on` is required only for the `poweroff`/`reboot` commands
(PSCI over HVC). The shell boots without it.

## Acceptance checklist

| Area | Criterion | Status |
| --- | --- | --- |
| Host tests | `cargo test --lib` all pass | **217/217** |
| Builds | `no_std`, `fog`, `fog-reloc`, `fog-auto-reset`, `fog-hw-selftests`, `tablet`, `smp-selftest`, `demo-tasks` | **all pass** |
| Console boot | SFS mounts, `howl ready`, no graphics tasks | **pass** |
| Shell | `help`, `uname`, `ls /`, `health`, file commands | **pass** |
| Init | Silverd resolves task ids (not cap ids) before grants | **pass** |
| Capabilities | `CAP_MOUNT`/`CAP_POWER` outside `CAP_ALL`; shell owns `CAP_POWER` by kernel policy | **pass** |
| Power | `poweroff`/`reboot`; SMC on Qualcomm, HVC on QEMU | **implemented; QEMU PSCI caveat below** |
| SMP | QEMU `-smp 4` selftest | **15 PASS / 0 FAIL** |
| Fog QEMU | relocation, at-link, fail-closed park, timer | **PASS** |
| SFS | mount, read/write, quotas, `du`, fsck | **pass (earlier checkpoint)** |
| Leaks | `health` reports `dma leaked 0`, `scopeprobe` status 0 | **pass (earlier checkpoint)** |
| Real hardware | console OS over UART on `rain` | **not verified (UART unclocked/unwired)** |

## Graphics removal

Removed with the subsystem: `src/display.rs`, `src/fog_ui.rs`, `src/gic_diag.rs`,
the kernel surface objects (`SYS_SURFACE_*`), `SYS_DISPLAY_INFO/MAP`,
`CAP_SURFACE`, the `fog-paint`/`fog-ui`/`fog-diag`/`fog-display` features,
`gpu_driver`, `input_driver`, `home`, `calc`, `gallery`, `notes`, the userspace
framebuffer/compositor/input-protocol modules, and the ramfb/`cont_splash`
paths. The document history below keeps the old evidence; it no longer
describes a supported build.

## Known gaps (ordered)

1. **QEMU PSCI shutdown does not terminate the VM in our test.** The HVC call
   reaches the emulated monitor and returns, but QEMU keeps running. The
   hardware SMC path is the one that matters for the phone. Keep the QEMU
   window/monitor as the host-side stop until this is root-caused.
2. **Fog MMU boot proofs are skipped by default.** The first TTBR0 TLB proof
   wedges a `rain` unit. The skip is explicit (`fog-hw-selftests` opts in);
   the proof must be debugged before claiming hardware MMU coverage.
3. **GENI UART root cause.** TX now fails fast after the first timeout, so boot
   no longer stalls, but the real clock/pin state is still unknown. This is the
   blocker for a console OS on the phone.
4. **Network tools are still thin.** The stack has ARP/IPv4/ICMP; DHCP, DNS,
   TCP sockets and user tools (`wget`-like) are the next layer.
5. **Unix-like toolbox is incomplete.** `ls`, `cat`, `echo`, `say`, `sl`,
   `bigdir`, `bigfile` exist; the plan is a busybox-style multi-call binary
   with `grep`, `wc`, `head`, `tail`, `find`, `cp`, `mv`, `rm`, `date`, `env`,
   `seq`, `true`, `false`, `basename`, `dirname`, `sleep`, `kill`, `ps`.

## Verification commands

```bash
cargo test --lib
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,smp-selftest
./redmi10c_poc/test_fog_qemu.sh
```

SMP run: `qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 4 -m 256M
-kernel <image> -display none -serial stdio`, expect 15 PASS and no
`SMP SELFTEST ... FAIL`.

Console run: boot the product image, then send `help`, `uname`, `ls /`,
`health` over the serial line. A machine without a NIC still boots; the
network driver reports a probe failure and the shell continues.

## Next work

1. Root-cause the QEMU PSCI shutdown or document a hard requirement.
2. Add the busybox-style Unix toolbox.
3. Grow networking: DHCP, DNS, TCP sockets, a `wget`-style tool.
4. Debug the fog TTBR0 TLB proof with `fog-hw-selftests`.
5. Make the GENI UART console usable on `rain` (clock/pins), then run the
   product console OS on the phone.
6. Only then: a minimal window + browser as a separate, self-contained
   deliverable.
