# Installing WerewolfOS ConsoleOS

ConsoleOS is the native configuration of WerewolfOS: the Silver microkernel
plus its own userspace (`silverd`, `howl`, `scout`, `net_server`, the Unix
toolbox), booting from an SFS image. This page is the install and quickstart
guide. It states plainly what runs today and what does not — an install guide
that lies about hardware support is worse than none.

- Supported today: **QEMU `virt`** (AArch64, Cortex-A72).
- Not supported yet: any physical board, including Raspberry Pi 4/5. See
  [Hardware boards](#hardware-boards) for the exact gap list.

## 1. What you need

| Component | Requirement |
|---|---|
| Host | Linux, macOS or Windows (x86_64 or AArch64) |
| QEMU | `qemu-system-aarch64` 7.0 or newer (`qemu-system-arm` package) |
| Rust | stable toolchain with the `aarch64-unknown-none` target (`rustup target add aarch64-unknown-none`) |
| GNU tar, sha256sum | present on any Linux/macOS host; on Windows use `tar` and `certutil -hashfile` |

You do not need a cross-compiler: the kernel, the userspace binaries and the
SFS image all build with Rust and the repo's own scripts.

## 2. Build a release bundle

```bash
git clone <this repository> silver-microkernel
cd silver-microkernel
scripts/make_release.sh --smoke
```

This builds the release kernel (`no_std,linux-abi`), packages it with the SFS
image, writes `MANIFEST` and `SHA256SUMS`, creates
`dist/werewolfos-<version>-consoleos-aarch64.tar.gz`, and then boots the
packaged artifacts in QEMU as a release gate. A non-zero exit code means the
bundle is not releasable.

Artifacts land in `dist/werewolfos-<version>/`:

| File | Contents |
|---|---|
| `silver_microkernel` | release kernel ELF for QEMU `virt` |
| `disk.img` | 64 MiB SFS image (OS, shell, Linux ABI test programs) |
| `MANIFEST` | version, features, toolchain, build time |
| `SHA256SUMS` | checksums of the three files above |

The version comes from `Cargo.toml` only; the kernel banner, `uname` and the
Linux `utsname` are generated from it at build time, so they cannot disagree.

## 3. Verify a download

```bash
sha256sum -c SHA256SUMS
```

On Windows PowerShell:

```powershell
Get-FileHash silver_microkernel, disk.img -Algorithm SHA256
```

Compare against `SHA256SUMS` and `MANIFEST` before running anything.

## 4. Run it

### Linux / macOS

```bash
cd dist/werewolfos-<version>
qemu-system-aarch64 \
    -machine virt -cpu cortex-a72 -smp 2 -m 1024M \
    -kernel silver_microkernel \
    -drive file=disk.img,format=raw,if=none,id=hd \
    -device virtio-blk-pci,drive=hd \
    -device virtio-net-pci,disable-legacy=on \
    -nographic
```

### Windows (PowerShell, one line)

```powershell
qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 2 -m 1024M `
    -kernel silver_microkernel `
    -drive file=disk.img,format=raw,if=none,id=hd -device virtio-blk-pci,drive=hd `
    -device virtio-net-pci,disable-legacy=on -nographic
```

`-smp 4` also works (SMP bring-up is covered by the `smp-selftest` build);
`-m 512M` is the minimum the boot self-tests run comfortably in.

Quit QEMU with `Ctrl-A`, then `X`.

### From the repository checkout

If you built from source and just want to boot, `./build_and_qemu.sh` does the
same with the tracked `disk.img` (and can start the Qt window viewer used by
the older Wolfser demo).

## 5. First steps in the shell

The prompt appears after the `fastfetch` banner:

```
howl> help
```

| Command | What it shows |
|---|---|
| `fastfetch` | OS/kernel version, uptime, memory, tasks, SFS usage, DMA state |
| `health` | frames, tasks, open files, DMA leaks — must stay at `zombie 0 / open 0 / leaked 0` |
| `ps` | tasks and their scheduler state |
| `ls`, `cat`, `echo`, `cp`, `grep`, `find`, `seq`, `du` | the Unix toolbox over SFS and `/tmp` |
| `spawn <program> [arg]` / `bg <program> [arg]` | start a task with / without the console |
| `bench [syscall\|sched\|fs\|vm\|memcpy\|all]` | cycle-accurate fast-path timings |
| `svc restart <name>` | restart a supervised service (`scout`, `net_driver`, `net_server`, `howl`, `howler`) |
| `linuxrun /sfs/linux-hello` | run a Linux ABI ELF (`hello from linux abi`, exit 42) |
| `poweroff`, `reboot` | PSCI power off / reset |

The image is writable: `echo hello > /sfs/note.txt` survives a reboot (verified
with an SFS fsck after the write: `python3 scripts/sfs_fsck.py disk.img`).

## 6. Hardware boards

**No physical board is supported by this release.** There is no downloadable
SD-card image and no board install procedure, because the drivers a board needs
do not exist yet. The console configuration boots on QEMU `virt` only.

Status of the candidate boards:

| Board | SoC | Status | Blocking work |
|---|---|---|---|
| QEMU `virt` | — | **Supported** | — |
| Raspberry Pi 5 | BCM2712 (Cortex-A76) | **Target**, not supported | Platform + linker base, UART/GIC/timer bases from the DTB, PSCI SMP, storage behind the RP1 southbridge/PCIe. Plan and milestone ladder: [PI5.md](PI5.md) |
| Raspberry Pi 4 Model B | BCM2711 (Cortex-A72) | Not supported | Same plan one SoC earlier; simpler storage path |
| Redmi 10C (`fog`) | SM6225 | Parked port | `fog.rs`/`platform.rs`/GENI UART merged behind feature flags; not bootable to userspace yet |

The first physical board this project aims at is a **Raspberry Pi 5**: a
console image needs only a UART, which keeps the bring-up small, and the board
is common enough that porting work can be verified by more than one person.
[PI5.md](PI5.md) lists what is known, what must be confirmed from the live
device tree, and the milestones (console log → timer/GIC → userspace from RAM →
storage → power).

What a Raspberry Pi port actually requires (this is the checklist, not a
promise of dates):

1. **Boot chain.** Pi 4/5 start the VideoCore firmware from EEPROM, which loads
   `start4.elf` + `config.txt` + our kernel. The kernel must be entered at EL1
   with the MMU off and be relocatable to the address the firmware chose; today
   `linker.ld` hardcodes DRAM at `0x4000_0000` (QEMU `virt`) and `linker_fog.ld`
   / `linker_tablet.ld` cover the parked ports. A Pi image needs its own linker
   script and either the firmware's `armstub` path or a U-Boot chainload.
2. **Platform detection.** `platform::detect()` recognises QEMU, SM8650 and
   SM6225 from the live device tree; a Pi would be a fourth `Kind`, with its
   peripheral bases taken from the DTB (`src/fdt.rs` already parses one).
3. **UART.** Pi 4 exposes a PL011 at `0xFE20_1000` (the mini-UART is at
   `0xFE21_5040`). `pl011.rs` drives a PL011, but at QEMU's fixed base; the base
   must come from the FDT.
4. **Interrupt controller.** Pi 4 uses a GIC-400 at `0xFF84_1000`. GICv2
   dispatch exists (`gic.rs`), again with QEMU's base hardcoded.
5. **Storage.** The SFS root lives on a block device. Pi 4/5 boot from SD/eMMC
   through an Arasan SDHCI controller — there is no driver for it yet, and
   without one there is no root filesystem, so this is the first real blocker
   after the console lights up.
6. **Power and time.** `poweroff`/`reboot` on a Pi go through the firmware
   mailbox (`0xFE00_B880`), not PSCI `SYSTEM_OFF`; the ARM generic timer works
   if `CNTFRQ_EL0` and the PPI are taken from the FDT.
7. **Display (optional for ConsoleOS).** A console image only needs the UART;
   HDMI would use the firmware framebuffer mailbox, which `simplefb.rs` could
   consume once the mailbox driver exists.

Contributions toward any of these are welcome — see `CONTRIBUTING.md`; the
repo's rule is that a platform claim must be backed by a boot log, not by a
configuration file.

## 7. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `qemu-system-aarch64: command not found` | install QEMU (`qemu-system-arm` on Debian/Ubuntu, `qemu` via Homebrew on macOS, the QEMU installer on Windows) |
| No prompt, no output | the serial console is the only output: do not use `-display` without `-serial`; `-nographic` is the simple choice |
| Boot stops at `SFS: mount` | the image is not a valid SFS: re-extract the bundle, then `python3 scripts/sfs_fsck.py disk.img` |
| `health` shows `zombie`/`open`/`leaked` non-zero | a leak regression: rerun `scripts/release_smoke.py` and attach its serial log |
| Boot environment mismatch | the release kernel targets QEMU `virt` with Cortex-A72; other machines need the port work above |
