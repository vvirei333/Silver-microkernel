# Final audit — zero-copy surfaces, bundle signing, DMA window
> **Исторический документ.** Графическая подсистема (композитор,
> surface objects, framebuffer, input/home) удалена 2026-11-08 после
> пересмотра вектора развития проекта. Описанные ниже графические части в
> дереве отсутствуют; документ сохранён как запись аудита.
>
> **Активный вектор:** консольная ОС, host-окно браузера (Qt), затем
> Linux ABI + Phosh. См. [`VECTOR.md`](VECTOR.md).


Date: 2026-11-07. Scope: the four remaining roadmap holes (fog DMA window,
kernel surface objects/zero-copy, bundle signatures, full audit).

## Closed holes

1. **Fog DMA window.** `USER_MMIO_L2_GROUPS` is now 8 (16 MiB) because the
   compositor holds a framebuffer, a legacy offscreen surface and a client
   surface at the same time. `map_shared_dma_to_user` no longer caps a shared
   run at one 2 MiB group. A boot self-test maps `ENTRIES + 1` pages across a
   group boundary and refuses an oversized run (`DMA WINDOW OK`), including on
   the fog builds.
2. **Kernel surface objects.** `SYS_SURFACE_CREATE/GRANT/MAP/REVOKE/SCANOUT`
   with a dedicated `CAP_SURFACE` (not `CAP_MMIO`). The owner keeps a writable
   alias; grantees map read-only; `SCANOUT` is a separate grant; frames pinned
   as scanout are never returned to the allocator, even when the holder dies
   (they are counted as leaked instead).
3. **Zero-copy present.** SilverHome draws 640x1136 pixels directly into its
   surface and sends `GUI_SURFACE_ATTACH`; the compositor re-points `ramfb` at
   that physical range. No pixel copy. Two surfaces are used when the ledger
   allows, so the displayed frame is not drawn into. Apps keep the legacy
   command-stream + blit path.
4. **Bundle signatures.** Ed25519 over the 32-byte SHA-256 digest (`sig=`,
   128 hex). `build.rs` embeds `keys/bundle_ed25519.pub` or
   `SILVER_BUNDLE_PUBKEY`; with a key every filesystem bundle needs a valid
   digest and signature. Without a key the kernel is integrity-only and
   refuses signatures it cannot authenticate.

## Audit findings fixed

- **Silently disabled boot self-tests.** TLB, ASID, heap-L3 and DMA-zero tests
  were wrapped in `if false &&` but still logged `OK`. They pass on QEMU now
  and run unconditionally; a failure stops the boot instead of lying.
- **Shell truncated signing commands.** `LINE_CAPACITY` was 96, shorter than
  `echo sig=<128 hex> >> <path>`; raised to 256.
- **Shell path check used the component limit.** `write_file`/`touch` compared
  the whole path against `NAME_MAX` (27), so `/sfs/apps/scopeprobe.manifest`
  was rejected; the check now uses the 191-byte path bound.
- **Fail-open malformed policy.** A malformed `quota=` line used to become "no
  quota"; malformed `sha256=`/`sig=` lines became "no constraint". They now
  set `BundlePolicy::invalid` and the bundle is refused (`InvalidPolicy`).
- **Shared-DMA one-group limit** (see hole 1) and the compositor window
  exhaustion it exposed.

## Evidence

- `cargo test --lib`: **218/218** (adds Ed25519 RFC 8032, signature policy,
  malformed-policy, SHA-256 and named-reserved-region vectors).
- Builds: `no_std`, `no_std,smp-selftest`, `no_std,tablet`,
  `no_std,fog,fog-reloc`, `no_std,fog,fog-ui`,
  `no_std,fog,fog-reloc,fog-display` — all pass.
- Phone display groundwork: with a 1 GiB synthetic khaje tree, the kernel logs
  `SILVER DISPLAY OK base=0x5C000000 mode=720x1650`, maps the panel through
  `SYS_DISPLAY_MAP`, and SilverHome pixels appear at `0x5C000000`; default
  `fog` boots without a display line.
- QEMU 640x1136: home screen 700/730 non-black samples; click Calc renders a
  different frame (730/730); back returns to the home frame; no panic.
- Boot log: `MMU TLB OK`, `MMU ASID OK`, `MMU HEAP OK`, `DMA ZERO OK`,
  `DMA WINDOW OK`, `SURFACE READY`, `howl ready`.
- `scopeprobe`: exit status 0, no frame/DMA/file leaks (`du` + `health`).
- SMP `-smp 4` selftest: TLB cross-PE, 64 stop-the-world rounds, ticks, BKL
  stall, watchdog and priority checks all PASS.
- Signed bundle: unsigned → `cannot load`; valid `sig=` → status 0; tampered
  signature → `cannot load`. `sfs_fsck` clean after every run.

## Known limitations

- Fog/tablet runtime was not executed here (no device boot): the builds and
  the new boot self-tests are present, but on-device execution remains a
  hardware check.
- Present fences are still best-effort `try_send`; a kernel fence object with
  waiting/guaranteed delivery is the next step.
- Only SilverHome uses zero-copy; installed apps still draw through the
  compositor command stream and a blit.
- No `SYS_SURFACE_DESTROY` yet: a surface lives until its owner exits.
- Key provisioning is manual and there is no key rotation/revocation story.
- SMP remains QEMU-only and opt-in; fog/tablet are single-core.
