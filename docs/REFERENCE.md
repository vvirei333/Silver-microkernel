# WerewolfOS reference console — acceptance and status

The product target is no longer a full graphical OS. It is a small, solid
console OS:

```text
Silver kernel
  + kernel SFS (/sfs, read-write)
  + nameserver (Scout)
  + network driver/server (optional device)
  + Howl shell + coreutils
  + Howler UART input
```

No compositor, no input tablet driver, no home screen. The graphics profiles
(`fog-display`, `fog-ui`, the ramfb compositor) remain in the tree as
experiments and are not part of the product boot.

This file is the reference checklist. "Done" means verified in this tree, not
merely implemented.

## Build and run

```bash
# product image (headless console)
cargo build --release --target aarch64-unknown-none \
  --no-default-features --features no_std,console

# QEMU smoke test
qemu-system-aarch64 -machine virt,virtualization=on -cpu cortex-a72 -smp 1 \
  -m 256M -kernel target/aarch64-unknown-none/release/silver_microkernel \
  -drive file=<sfs.img>,format=raw,if=none,id=hd \
  -device virtio-blk-pci,drive=hd -display none -serial stdio
```

`virtualization=on` is required only for the `poweroff`/`reboot` commands
(PSCI over HVC). The shell itself boots without it.

## Acceptance checklist

| Area | Criterion | Status |
| --- | --- | --- |
| Host tests | `cargo test --lib` all pass | **218/218** |
| Builds | `no_std`, `console`, `fog`, `fog-reloc`, `fog-ui`, `fog-display`, `tablet`, `smp-selftest` | **all pass** |
| Console boot | SFS mounts, `howl ready`, no graphics tasks | **pass** |
| Shell | `help`, `uname`, `ls /`, `health`, file commands | **pass** |
| Init | Silverd resolves task ids (not cap ids) before grants | **fixed** |
| Capabilities | `CAP_MOUNT`/`CAP_POWER` outside `CAP_ALL`; console shell gets `CAP_POWER` by kernel policy only | **pass** |
| Power | `poweroff`/`reboot` commands; SMC on Qualcomm, HVC on QEMU | **implemented; fog path verified earlier, QEMU PSCI caveat below** |
| SMP | QEMU `-smp 4` selftest | **15 PASS / 0 FAIL** |
| Fog QEMU | relocation, at-link, fail-closed park, timer | **PASS** |
| SFS | mount, read/write, quotas, `du`, fsck | **pass (earlier checkpoint)** |
| Leaks | `health` reports `dma leaked 0`, `scopeprobe` status 0 | **pass (earlier checkpoint)** |
| Real hardware | console OS over UART on `rain` | **not verified (UART unclocked/unwired)** |

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
4. **Network without a NIC.** The console profile still starts the network
   driver/server; a machine without a virtio-net device reports a probe failure
   and continues. A `console-net` split is the next cleanup if the product
   should not even embed the stack.
5. **Graphics code stays in the kernel image.** `GPU_DRIVER`, `INPUT_DRIVER`
   and `HOME` are still embedded by `user_program.rs` even in the console
   build. They are not started, but a size-conscious console build should
   compile them out.

## Verification commands

```bash
cargo test --lib
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,console
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,smp-selftest
./redmi10c_poc/test_fog_qemu.sh
```

SMP run: `qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 4 -m 256M
-kernel <image> -display none -serial stdio`, expect 15 PASS and no
`SMP SELFTEST ... FAIL`.

Console run: boot the product image, then send `help`, `uname`, `ls /`,
`health` over the serial line. `VIRTIO-INPUT FAILURE STAGE 1` is expected when
no tablet device is attached; it is not a product failure.

## Next work

1. Root-cause the QEMU PSCI shutdown or document a hard requirement.
2. Compile graphics programs out of the console build.
3. Add a `console-net` split and make the default console boot deterministic
   without a NIC.
4. Debug the fog TLB proof with the `fog-hw-selftests` image.
5. Make the GENI UART console usable on the phone (clock/pins), then run the
   product boot on `rain` over UART.
