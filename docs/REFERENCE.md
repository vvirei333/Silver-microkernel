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
| Wolfser | `wolfser [query|url]`, search/address bar, clickable links, per-tab history, six cached tabs, **litehtml CSS layout** with MediaWiki/`main`/`article` reader extraction (48 KiB slice), find-in-page, start page with persistent bookmarks, one-field GET/POST forms with a session cookie jar, hover status/hand cursor, middle-click new tab, host zoom, redirects; HTTP and HTTPS TLS 1.3 via smoltcp, Qt proxy as fallback; `--snapshot` sees a rendered frame | **pass** |
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
4. **Networking has a native TLS layer.** DHCP, DNS, TCP, HTTP and TLS 1.3
   (`ip`, `dns`, `wget https://`, Wolfser) work in the guest. The client uses
   six roots, RSA/ECDSA/Ed25519 and a fixed certificate date until the RTC;
   unsupported sites fall back to the viewer's Qt TLS proxy. HTTP/2, parallel
   fetches and a DNS TTL from the wire remain. The client body window is 1 MiB
   and the user stack is now 64 KiB to fit the TLS state machine.
5. **Wolfser has a real HTML/CSS engine.** litehtml (BSD-3) + gumbo lay out
   pages with block/inline boxes, margins, colours, lists, borders and link
   hit-testing. MediaWiki/`main`/`article` pages use a sanitized reader view;
   other pages keep inline CSS and small external stylesheets. Six tabs,
   find-in-page, persistent bookmarks, host zoom and one-field GET/POST forms
   work. Images, JavaScript, media, larger reader/CSS budgets and full-page
   layout do not yet; the Linux ABI/Phosh track brings a WebKit/Gecko-class
   engine for those and for YouTube.
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

1. Harden TLS: more roots, RTC-backed certificate time, a real entropy source
   and a TLS 1.2 fallback.
2. Wolfser: in-guest image decoding, larger reader/CSS budgets, full
   forms/widgets, keep-alive and damage rectangles.
3. Window transport on real hardware (host-side streaming or a local console).
4. Linux ABI: static musl binaries, then busybox `sh`.
5. Phosh: headless Wayland, GTK software render, D-Bus + logind stub, `phosh`.
6. Parked: QEMU PSCI shutdown, fog TLB proof, GENI UART on `rain`, tablet.
