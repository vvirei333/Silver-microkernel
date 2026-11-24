# Reference board: Raspberry Pi 5 (BCM2712)

Status: **target, not supported.** No image exists for this board and none of
the milestones below is done. This page is the plan and the checklist, so that
work can be measured against hardware instead of against hope.

WerewolfOS is developed on QEMU `virt` (Cortex-A72). Raspberry Pi 5 is the
intended first physical board: AArch64, well documented device trees, a
PL011 debug UART on the header (so the console product needs no display), and
hardware that people actually own. Raspberry Pi 4 remains the fallback board —
the same plan applies one step earlier in the SoC family.

## What is known, and what must be confirmed on hardware

| Topic | What we rely on | Confidence |
|---|---|---|
| CPU | 4× Cortex-A76, AArch64, EL2 → EL1 handoff from firmware | known |
| Boot | EEPROM bootloader → `config.txt`/`cmdline.txt` → arm64 kernel image or `armstub8`; the load address and entry contract come from the firmware, not from us | known shape, exact address to verify |
| Console | PL011 debug UART (uart10 on a Pi 5) at 115200 8N1 on the header | known shape; base, INTID and reference clock are read from the node, including the Pi's `clocks = <&clk_uart>` phandle form |
| Interrupts | GIC-400, ARM generic timer PPI | known shape; distributor and CPU-interface bases and the timer PPI INTID are all read from the tree (verified on QEMU's tree) |
| Secondary cores / power | PSCI via firmware | expected, to be confirmed |
| Storage | the Pi 5 microSD slot is the BCM2712 `sdio1` controller (`brcm,bcm2712-sdhci`, `reg = <0x10 0x00fff000 ...>`, IRQ 273). RP1 contains `rp1_mmc0`/`rp1_mmc1`, but both are `disabled` in the Pi 5 tree. RP1 is still needed for USB and Ethernet, not for the SD card. | controller and tree shape known; no hardware log yet |
| Firmware services | mailbox for poweroff/reboot, framebuffer (not needed for a console image) | known shape, to be implemented |

Two rules follow from the right-hand column:

1. **No peripheral base is hardcoded for a board.** The kernel already parses a
   live FDT (`src/fdt.rs`); a board port takes UART, GIC and timer bases from
   it. QEMU stays the reference while that refactor lands.
2. **A milestone is only claimed with a serial log.** "It should work" is not a
   status.

## Milestones

| # | Milestone | Acceptance | Blocking work |
|---|---|---|---|
| P0 | Firmware hands over to the kernel and the UART prints | a boot log on the header UART through the first `console::init` line | ~~platform `Kind` for BCM2712~~ (done), ~~PL011 base and reference clock from the DTB~~ (done), ~~link base as a build parameter~~ (done), remaining: confirm the firmware's `kernel_address`/armstub handoff and capture the first serial log; the stub already drops EL2 → EL1 when the firmware enters there |
| P1 | Interrupts and time | timer tick, GIC init, `health`/`uptime` advancing, no fault | ~~GIC-400 base from the DTB~~ (done), ~~timer PPI selection~~ (done), ~~PSCI conduit and per-CPU MPIDR from the tree~~ (done), remaining: a hardware run |
| P2 | Console userspace from RAM | `howl>` prompt and `help` **with no storage at all** | already reachable on QEMU with no disk: the shell and its commands are embedded in the kernel; remaining: one Pi 5 serial log |
| P3 | Root filesystem on the board | `ls /sfs` reads the real image; `cat`, `spawn`, `bench fs` work | ~~PCIe host discovery and RP1 identification~~ (software); ~~block-device layer and polled SDHCI core, including the BCM2712 `sdio1` pin mux and 4-bit/25 MHz bring-up~~ (software, not hardware-verified); remaining: one Pi 5 serial log with an actual card, and RP1 USB/Ethernet if SD is unavailable |
| P4 | Power control | `poweroff`/`reboot` from the shell | ~~PSCI conduit from the tree~~ (done); remaining: confirm `SYSTEM_OFF`/`SYSTEM_RESET` over SMC on the board (use the mailbox only if firmware lacks PSCI power calls) |
| P5 | Network (optional) | `ping`/`wget` over the on-board Ethernet | RP1 Ethernet + the existing virtio-free userspace stack |

P2 is deliberately before P3: it proves the console product on real hardware
without waiting for the storage stack, which is the riskiest part of the board.

## What is actually left, in order

1. P0/P1 hardware evidence: build the image for the firmware's `kernel_address`,
   boot it, and attach the serial log. The tree-first console, GIC, timer, PSCI
   conduit selection and AP MPIDR mapping are in the kernel now, but none of
   them has been observed on a Pi 5.
2. P3 storage: PCIe host discovery exists (QEMU ECAM and the BCM2712
   index/data back end), RP1 is recognized by `1de4:0001`, and SFS now sits on
   a small block layer with a polled SDHCI back end. The Pi 5 path is BCM2712
   `sdio1`, not RP1: the driver selects an enabled `brcm,bcm2712-sdhci` node,
   applies the SD pin mux and brings the card up at 400 kHz, then 4-bit at
   25 MHz. QEMU still uses virtio-blk. What is missing is hardware evidence
   with a real card, plus RP1 USB/Ethernet for the other I/O.
3. P4: check whether the Pi firmware implements PSCI `SYSTEM_OFF` and
   `SYSTEM_RESET`; only add mailbox power control if it does not.

## The tree-first path is verified, not assumed

Booting a flat Image (not an ELF) hands the kernel a real device tree, and the
kernel binds the console from it:

```bash
python3 scripts/make_image.py                       # ELF -> dist/silver.img
qemu-system-aarch64 -machine virt,dumpdtb=/tmp/virt.dtb -cpu cortex-a72 \
  -smp 2 -m 1024M -display none                     # produce a tree
qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 2 -m 1024M -display none \
  -dtb /tmp/virt.dtb -kernel dist/silver.img \
  -drive file=disk.img,format=raw,if=none,id=hd -device virtio-blk-pci,drive=hd \
  -serial mon:stdio
```

boot log:

```
PLATFORM=QEMU
DRIVER UART=DT base=0x0000000009000000 size=0x0000000000001000 irq=33
```

No address in that line came from the kernel: the UART base, its size and its
INTID were read from the tree, the console transport was chosen by
`choose_console`, and `pl011` was initialised with the base it was handed. The
same boot reaches `howl>`, runs `health` (0 zombie, 0 open, 0 leaked DMA) and
`bench fs` (SFS open+close ~19-23 µs), so the tree-first path is a working
console, not a special diagnostic mode.

### The load address is a build parameter, not a patched linker script

`linker.ld` keeps the QEMU defaults and accepts overrides, so a board build is
one environment variable away:

```bash
SILVER_LINK_PA=0x80000 SILVER_DRAM_BASE=0x0 cargo kernel   # example: load at 0x80000
python3 scripts/make_image.py                              # reads __kernel_link_pa/__dram_base
```

`build.rs` validates the values (decimal or `0x`, page aligned) and passes them
as `--defsym`; the linker script uses them only when defined, so the default
build is bit-for-bit the same placement as before. Verified by linking a kernel
at `0x4020_0000`, producing the matching `text_offset = 0x200000` Image and
booting it: the shell comes up and `health` is clean, so the mechanism is not
just a build flag.

### The timer PPI comes from the tree as well

The non-secure physical timer's INTID was a platform constant (30 on QEMU,
18 on khaje). It is now bound from the `arm,armv8-timer` node like every other
device, with the old constants kept only as a no-tree fallback. Same
property-order trap as the GIC: `dtc` writes `interrupts` *before*
`compatible`, so the timer read as absent until the parser stopped trusting
property order (now covered by a regression test built from QEMU's real cell
layout). Boot evidence:

```
DRIVER GIC=DT dist=0x0000000008000000 cpu=0x0000000008010000
DRIVER TIMER=DT intid=30
TIMER TEST: ticks=30
```

### The interrupt controller is a device, too

`gic` no longer assumes QEMU's 0x0800_0000/0x0801_0000: `driver_resources` binds
the distributor and the GICv2 CPU interface from the tree's GIC node (`reg`
pairs 1 and 2) and `gic::init_at(dist, cpu)` takes both. They are separate
parameters because they are not at a fixed offset from each other on real
controllers.

Verifying this found a live parser bug: `dtc` sorts properties, so `reg` comes
*before* `compatible`, and a GIC node also has children (QEMU's carries a `v2m`
frame). The visitor pattern used one shared state per finder, so a child's
`begin` erased the parent's properties before the parent's `end` ran — the
controller was silently ignored on a *real* device tree while the synthetic
tests (which wrote `compatible` first and no children) passed. The finders now
keep one state per nesting level, and two regression tests cover the real
property order and the child-node case. Boot evidence:

```
DRIVER UART=DT base=0x0000000009000000 size=0x0000000000001000 irq=33 clk=24000000
DRIVER GIC=DT dist=0x0000000008000000 cpu=0x0000000008010000
IRQ READY
```

### The baud divisor follows the node's clock

The UART's divisor is a function of its reference clock, so `pl011` no longer
hardcodes QEMU's 24 MHz. `fdt` reads `clock-frequency` (or the first
`assigned-clock-rates` cell) from the console node within a plausibility range,
`driver_resources` stores it, and the console calls `init_at_clock`. Booting a
tree that declares 48 MHz prints it back:

```
DRIVER UART=DT base=0x0000000009000000 size=0x0000000000001000 irq=33 clk=48000000
```

The Raspberry Pi device trees name that clock by phandle
(`clocks = <&clk_uart>`), whose rate lives on the referenced fixed-clock node.
`fdt` resolves the phandle to the provider node and reads its rate (with the
same plausibility bound), and a node's own `clock-frequency` wins over the
phandle. This is verified on QEMU's unpatched tree, which uses the same spelling
(`clocks = <0x8000 ...>` → `apb-pclk`):

```
DRIVER UART=DT base=0x0000000009000000 size=0x0000000000001000 irq=33 clk=24000000
```

## What a board owner can do today

The most useful contributions are *data from real hardware*, not patches:

1. Dump the device tree the firmware passes to the kernel:
   ```bash
   dtc -I dtb -O dts /boot/firmware/bcm2712-rpi-5-b.dtb > bcm2712-rpi-5-b.dts
   ```
   The UART, GIC, timer and the PCIe/RP1 nodes are what the port needs. Attach
   the `.dts` to an issue.
2. Capture the firmware's own boot log over the header UART (`enable_uart=1`,
   115200) from power-on to the first Linux line — that is the handover the
   kernel has to survive.
3. Report `vcgencmd version`, the firmware config (`config.txt`) and whether
   the board is a 4 GB/8 GB model.

## Non-goals for the board bring-up

- No HDMI/graphics claim: the console configuration is UART-first. The Denial
  desktop track stays QEMU-only until a console image boots on hardware.
- No promise of a date, and no SD-card install procedure until P3 is a real
  serial log.

## Progress log

| Date | Change | Evidence |
|---|---|---|
| 2026-11-25 | SFS is no longer welded to virtio-blk. `block` owns one synchronous 512-byte device and `init` picks SDHCI first, then virtio-blk only when the tree has an ECAM host (or no tree). The polled SDHCI core is generic; `brcm,bcm2712-sdhci` adds the BCM2712 SD pin mux and 200 MHz base-clock fallback. Pi 5 microSD is BCM2712 `sdio1`, not RP1; the RP1 MMC nodes are disabled. The BCM peripheral mapping grows from 2 MiB to 16 MiB so `0x10_00fff000` is reachable. | 268 host tests; no_std and QEMU boots with and without a DTB still select `virtio-blk`; SDHCI command/card path is software-only, no Pi 5 log |
| 2026-11-24 | PCIe discovery is tree-driven. `pci` parses `pci-host-ecam-generic` and `brcm,bcm2712-pcie`, and an MMIO backend reads either ECAM or the BCM2712 index (`0x9000`) / data (`0x8000`) registers. Enumeration records vendor/device/class, skips disabled hosts, and recognizes RP1 as `1de4:0001` at `01:00.0`. The BCM controller block is mapped as Device memory in the kernel page tables. | 261 host tests; `no_std` build; flat-Image QEMU boot with a real DTB logs `PCI: host=ECAM`, four functions and the shell still runs; BCM/RP1 path is covered by parser and scanner tests, not hardware |
| 2026-11-24 | PSCI conduit and AP MPIDRs come from the live tree instead of the board name and CPU index. `fdt` reads `/psci/method` (`hvc`/`smc`) and each `/cpus/cpu*` `reg`; `power` caches the conduit and issues both power and `CPU_ON` through it. This is what the Pi 5 needs (`smc`, clustered affinity); QEMU keeps `hvc` and the index fallback. Also confirmed the no-storage console path already works: QEMU without any disk reaches `howl>`, `help` and `health`. | 254 host tests; no_std and `smp-selftest` builds; `-smp 4` boot 11/11 self-tests; no-disk serial log shows `howl>` + `help` + `health` |
| 2026-11-23 | Timer PPI from the tree: INTID bound from `arm,armv8-timer` (`gic::set_timer_irq`), platform constant only as a no-tree fallback; fixed the same property-order bug in `TimerFinder` (QEMU writes `interrupts` first). | regression test with the real cell layout; 249 host tests; smoke 8/8; `smp-selftest` 10/10; boot log `DRIVER TIMER=DT intid=30` |
| 2026-11-23 | Interrupt controller from the tree: GIC distributor and CPU-interface bases bound from the node (`gic::init_at`), no fixed offset assumed. Found and fixed a real visitor bug (property order `reg` before `compatible`, and children resetting the parent's captured state) — the GIC was silently ignored on any real tree. | 2 regression tests (real property order, matching node with a child); 248 host tests; smoke 8/8; `smp-selftest` 10/10 on `-smp 4`; boot log shows `DRIVER GIC=DT` |
| 2026-11-23 | UART clock phandle resolution: `clocks = <&clk_uart>` is followed to the provider node and its rate read, so the Pi's own spelling works without vendor-specific code. Verified on QEMU's stock device tree (`clk=24000000` from `apb-pclk`, reached only through the phandle) with the shell still running. | 2 new `fdt` tests (phandle resolution incl. unknown phandle, direct value wins); 246 host tests; smoke 8/8 |
| 2026-11-23 | Load address is a build parameter: `SILVER_LINK_PA` / `SILVER_DRAM_BASE` / `SILVER_KERNEL_VIRT_BASE` → `--defsym`, with `DEFINED()` guards in `linker.ld`; `scripts/make_image.py` now reads `__dram_base` from the ELF instead of assuming QEMU. Verified by booting an image linked at 0x4020_0000 (`text_offset = 0x200000`) through the Image protocol: console, shell and `health` all work; the default build boots unchanged (smoke 8/8). | 244 host tests, five feature builds, smoke 8/8, custom-base boot log |
| 2026-11-23 | Baud divisor from the device tree: `fdt` parses `clock-frequency`/`assigned-clock-rates` on the console node (bounded to 1 MHz..1 GHz, so a wild value cannot become a register write), `driver_resources` stores it, `pl011::init_at_clock` uses it. Verified by booting a tree that declares 48 MHz: the log line carries `clk=48000000` and the console still runs. Phandle-only clocks remain unresolved (documented above). | 4 new `fdt` tests (read, `assigned-clock-rates`, unset, implausible); 244 host tests total; smoke 8/8 |
| 2026-11-23 | Tree-first console verified end to end: `scripts/make_image.py` turns the ELF into an AArch64 Image with a correct `text_offset`; booting it with `-dtb` prints `DRIVER UART=DT base=0x9000000 ... irq=33`, then serves `howl>`/`health`/`bench fs`. **Correction:** an earlier note here claimed the generic boot stub does not record `x0`; that was wrong — it does (`str x22, [x3, #0x28]` in the common publish block). QEMU's *ELF* load path passes no tree, which is why the ELF boot shows no `DT` line. | raw-Image boot log above; bench 19.2-23.2 µs open+close on both paths (same code), `-smp 4` self-tests unchanged |
| 2026-11-23 | The console decision is now pure, tested policy: `choose_console(described, builtin)` in `src/devices.rs` — tree wins, built-in map is only a fallback, nothing described means silence. `fdt` moved from being compiled twice into a single library module the binary re-exports, so the parser the tests cover is the parser the kernel runs. | 240 host tests (4 policy tests: "the tree wins over the built-in map", "a tree alone is enough", "the built-in map is only a fallback", "nothing described means no console") |
| 2026-11-23 | Devices are chosen from the tree before the board: `driver_resources` is the one device map, `console::active()` asks it first, and `pl011` is now address-agnostic (`init_at(base)`, divisor from a UART clock). `mmio_range`/`irq` no longer branch on a board name — a console driver gets the UART, a probe gets the GIC, whoever they are. | 236 host tests; all six `secprobe` cases `ok` (incl. `irq_orphan`, `mmio_range_confine`); release smoke 8/8; `no_std`, `linux-abi`, `fog`, `tablet` builds |
| 2026-11-23 | `Kind::Bcm2712` exists and is proved from the DT root `compatible` (`brcm,bcm2712`, `raspberrypi,5-*`) by `kind_from_compatible` in `src/board.rs`; MIDR deliberately does not map Cortex-A76, so no board can be guessed from the CPU. `console.rs` maps BCM2712 to "no output" until the PL011 base is passed in. | 7 host tests (`cargo test --lib board`) including "Pi 4 is not claimed as a Pi 5" and "a substring is not a match"; QEMU boot unchanged (release smoke 8/8) |
