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
2026-11-08 after reconsidering the project's development vector.

Active vector (see [`VECTOR.md`](VECTOR.md)):

- console OS on QEMU/desktop;
- a browser in a separate window rendered by a Qt host over a shared surface;
- **Linux ABI + Phosh** as the active long-term goal (owner decision:
  months, not years), with a software-rendered headless Wayland path.

The Legion Y700 tablet target is forgotten. The Redmi port is parked.

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
| Builds | `no_std`, `fog`, `fog-reloc`, `fog-auto-reset`, `fog-hw-selftests`, `smp-selftest`, `demo-tasks` (tablet compiles, parked) | **all pass** |
| Console boot | SFS mounts, `howl ready`, no graphics tasks | **pass** |
| Shell | `help`, `uname`, `ls /`, `health`, file commands | **pass** |
| Init | Silverd resolves task ids (not cap ids) before grants | **pass** |
| Capabilities | `CAP_MOUNT`/`CAP_POWER` outside `CAP_ALL`; shell owns `CAP_POWER` by kernel policy | **pass** |
| Toolbox | multi-call `utils`: grep/wc/head/tail/cp/find/seq/dirname/sleep/true/false | **pass** |
| Wolfser | `wolfser [query|url]`, search/address bar, clickable links, per-tab history, four cached tabs, redirects; HTTP via smoltcp, HTTPS via the Qt host proxy; `--snapshot` sees a rendered frame | **pass** |
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
4. **Networking has its first full layer.** DHCP, DNS, TCP and an HTTP client
   are working (`ip`, `dns`, `wget`); HTTP/2 and parallel fetches remain. The
   browser reaches HTTPS through the host viewer's Qt TLS proxy (the guest
   publishes a URL, the host returns the body); a native in-guest TLS stack is
   still to come.
5. **Wolfser is a real text browser.** Search/address bar, clickable links,
   back/forward history, four tabs with cached pages, 3xx and meta-refresh
   redirects, word wrap, headings, reader-mode content selection and a moving
   loading screen work. CSS layout, images, forms, bookmarks and more tabs do
   not yet; the Linux ABI/Phosh track replaces the renderer with a real engine
   later.
6. **Linux ABI/Phosh are not started.** The kernel personality layer, static
   musl binaries, headless Wayland and the Phosh stack are the active
   long-term track; see [`VECTOR.md`](VECTOR.md).
7. **Unix toolbox, first layer done.** A multi-call `utils` ELF provides
   `grep`, `wc`, `head`, `tail`, `cp`, `find`, `seq`, `basename`, `dirname`,
   `true`, `false` and `sleep`; `ls`, `cat`, `echo`, `say`, `sl` remain
   separate programs. Next: `date` (needs RTC), `env`, a real pager, and
   folding the remaining utilities into the same binary.

## Third-party notes

`smoltcp` 0.14.0 is vendored in `third_party/smoltcp` with two guarded early
returns in the neighbour path (`PATCH.md`): a DHCP boot could otherwise ask the
neighbour cache about `255.255.255.255` and hit its unicast assertion. The
0BSD licence is kept beside the source.

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
`health` over the serial line. Wolfser: `./build_and_qemu.sh --viewer`, then
`wolfser` in the guest; the Qt window works under KDE/GNOME on Wayland or X11,
and `https://` pages are fetched by the viewer's Qt Network module. A machine
without a NIC still boots; the
network driver reports a probe failure and the shell continues.

## Next work

1. Native TLS client (rustls/embedded-tls) so `wget` and the browser do not
   depend on the host viewer for `https://`.
2. Wolfser: images (PNG first), bookmarks/start page, hover link targets,
   damage rectangles, and more than four tabs.
3. Window transport on real hardware (host-side streaming or a local console).
4. Linux ABI: static musl binaries, then busybox `sh`.
5. Phosh: headless Wayland, GTK software render, D-Bus + logind stub, `phosh`.
6. Parked: QEMU PSCI shutdown, fog TLB proof, GENI UART on `rain`, tablet.
