# Contributing

## Ground rules

1. **Verify before claiming.** A feature is "done" when a boot self-test or
   a reproducible QEMU run proves it. Unverified work is marked
   EXPERIMENTAL in code comments and docs.
2. **Fail closed.** New resource paths (memory, IRQ, DMA, scheduling) need a
   policy check. A path that cannot be checked must refuse.
3. **No distribution dependencies in the kernel.** Image builders stage
   Alpine/Debian trees; the kernel crate must build and boot without them.
4. **Match the surrounding code.** Comment density and naming follow the
   module you edit; diagnostics go into `no_mangle` rings or the console
   debug prints, never into `log`-style frameworks.
5. **No vendored third-party code** without a license review (see
   DESIGN_DECISIONS.md D11 on BSD ideas).

## Development environment

- Windows or Linux host, Rust nightly for `aarch64-unknown-none`.
- ARM cross-binutils (nm/readelf) for kernel symbol work — the repo uses
  `WOLF_ARM_TOOLCHAIN` pointing at an aarch64-none-elf toolchain.
- QEMU (`-machine virt -cpu cortex-a72 -smp N`), serial over TCP.

## Kernel build

```
cargo build --release --target aarch64-unknown-none \
  --no-default-features --features no_std,linux-abi,gdb-stub
```

Feature flags are documented in `Cargo.toml` and the README; each opt-in
feature is fail-closed when absent.

## Images and testing

- `tools/build_glibc_denial.py` — the Mobile configuration image
  (Alpine boot chain + Debian arm64 runtime + Denial WM).
- `tools/boot_dump.py` — boots an image, waits for a marker, takes a
  pmemsave dump via the QEMU monitor (`--dump`, `--dump-size`, `--timeout`).
- `tests/linux/` — freestanding Linux-ABI probes; add one per fixed bug
  where feasible.

## Debug workflow (kernel + Linux stack)

1. Reproduce with `boot_dump.py`, keep the serial log.
2. Kernel-side prints (`OPENFAIL`, `EXECFAIL`, `SOCKDBG`, `FAULTCTX`,
   `LINUX ERR`) usually pinpoint the mismatched syscall/errno.
3. TRACE_RING holds the last guest syscalls per task; parse it from a dump
   (ring addresses via `nm` after every rebuild — they move).
4. Never trust an fd argument you have not cross-checked against the ring's
   open/close history.

## Documentation

Architecture docs live in `docs/`. When behavior changes, update the
relevant doc in the same change; STATUS.md carries the verification markers
(IMPLEMENTED / PARTIALLY IMPLEMENTED / EXPERIMENTAL / PLANNED).

## Submitting

Small, self-contained changes; one concern per change. Include the
reproduction command (boot invocation or test binary) in the description.
