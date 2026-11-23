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
| Console | PL011 debug UART (uart10 on a Pi 5) at 115200 8N1 on the header | known shape; base, INTID and reference clock are read from the node (note: the Pi tree names its clock by phandle, which the parser does not resolve yet — see below) |
| Interrupts | GIC-400, ARM generic timer PPI | known shape, bases to be read from the DTB |
| Secondary cores / power | PSCI via firmware | expected, to be confirmed |
| Storage | most I/O on Pi 5 sits behind the **RP1 southbridge over PCIe**; the SD/eMMC path must be confirmed from the live DTB (`lspci` on a running Linux shows the RP1) | **unknown — the main risk** |
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
| P0 | Firmware hands over to the kernel and the UART prints | a boot log on the header UART through the first `console::init` line | ~~platform `Kind` for BCM2712~~ (done), ~~PL011 base and reference clock from the DTB~~ (done), ~~link base as a build parameter~~ (done), remaining: the Pi firmware's entry contract (EL2/armstub, secondary-core release) and confirming its `kernel_address` |
| P1 | Interrupts and time | timer tick, GIC init, `health`/`uptime` advancing, no fault | GIC-400 base from the DTB, timer PPI selection, PSCI SMP bring-up |
| P2 | Console userspace from RAM | `howl>` prompt and `help` **with no storage at all** (SFS image embedded or seeded into RamFs) | embedding a minimal SFS into the kernel image, platform-independent VFS init |
| P3 | Root filesystem on the board | `ls /sfs` reads the real image; `cat`, `spawn`, `bench fs` work | storage path: RP1/PCIe + SD, or USB mass storage — whichever the DTB shows |
| P4 | Power control | `poweroff`/`reboot` from the shell | firmware mailbox driver |
| P5 | Network (optional) | `ping`/`wget` over the on-board Ethernet | RP1 Ethernet + the existing virtio-free userspace stack |

P2 is deliberately before P3: it proves the console product on real hardware
without waiting for the storage stack, which is the riskiest part of the board.

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

### The baud divisor follows the node's clock

The UART's divisor is a function of its reference clock, so `pl011` no longer
hardcodes QEMU's 24 MHz. `fdt` reads `clock-frequency` (or the first
`assigned-clock-rates` cell) from the console node within a plausibility range,
`driver_resources` stores it, and the console calls `init_at_clock`. Booting a
tree that declares 48 MHz prints it back:

```
DRIVER UART=DT base=0x0000000009000000 size=0x0000000000001000 irq=33 clk=48000000
```

**Known gap, stated rather than hidden:** the Raspberry Pi device trees name the
UART clock by phandle (`clocks = <&clk_uart>`), whose rate lives on the
referenced fixed-clock node. This parser does not resolve phandles yet, so on a
Pi the driver would fall back to the documented default until that lands (or
until the firmware/`config.txt` sets the UART clock to the assumed value).

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
| 2026-11-23 | Load address is a build parameter: `SILVER_LINK_PA` / `SILVER_DRAM_BASE` / `SILVER_KERNEL_VIRT_BASE` → `--defsym`, with `DEFINED()` guards in `linker.ld`; `scripts/make_image.py` now reads `__dram_base` from the ELF instead of assuming QEMU. Verified by booting an image linked at 0x4020_0000 (`text_offset = 0x200000`) through the Image protocol: console, shell and `health` all work; the default build boots unchanged (smoke 8/8). | 244 host tests, five feature builds, smoke 8/8, custom-base boot log |
| 2026-11-23 | Baud divisor from the device tree: `fdt` parses `clock-frequency`/`assigned-clock-rates` on the console node (bounded to 1 MHz..1 GHz, so a wild value cannot become a register write), `driver_resources` stores it, `pl011::init_at_clock` uses it. Verified by booting a tree that declares 48 MHz: the log line carries `clk=48000000` and the console still runs. Phandle-only clocks remain unresolved (documented above). | 4 new `fdt` tests (read, `assigned-clock-rates`, unset, implausible); 244 host tests total; smoke 8/8 |
| 2026-11-23 | Tree-first console verified end to end: `scripts/make_image.py` turns the ELF into an AArch64 Image with a correct `text_offset`; booting it with `-dtb` prints `DRIVER UART=DT base=0x9000000 ... irq=33`, then serves `howl>`/`health`/`bench fs`. **Correction:** an earlier note here claimed the generic boot stub does not record `x0`; that was wrong — it does (`str x22, [x3, #0x28]` in the common publish block). QEMU's *ELF* load path passes no tree, which is why the ELF boot shows no `DT` line. | raw-Image boot log above; bench 19.2-23.2 µs open+close on both paths (same code), `-smp 4` self-tests unchanged |
| 2026-11-23 | The console decision is now pure, tested policy: `choose_console(described, builtin)` in `src/devices.rs` — tree wins, built-in map is only a fallback, nothing described means silence. `fdt` moved from being compiled twice into a single library module the binary re-exports, so the parser the tests cover is the parser the kernel runs. | 240 host tests (4 policy tests: "the tree wins over the built-in map", "a tree alone is enough", "the built-in map is only a fallback", "nothing described means no console") |
| 2026-11-23 | Devices are chosen from the tree before the board: `driver_resources` is the one device map, `console::active()` asks it first, and `pl011` is now address-agnostic (`init_at(base)`, divisor from a UART clock). `mmio_range`/`irq` no longer branch on a board name — a console driver gets the UART, a probe gets the GIC, whoever they are. | 236 host tests; all six `secprobe` cases `ok` (incl. `irq_orphan`, `mmio_range_confine`); release smoke 8/8; `no_std`, `linux-abi`, `fog`, `tablet` builds |
| 2026-11-23 | `Kind::Bcm2712` exists and is proved from the DT root `compatible` (`brcm,bcm2712`, `raspberrypi,5-*`) by `kind_from_compatible` in `src/board.rs`; MIDR deliberately does not map Cortex-A76, so no board can be guessed from the CPU. `console.rs` maps BCM2712 to "no output" until the PL011 base is passed in. | 7 host tests (`cargo test --lib board`) including "Pi 4 is not claimed as a Pi 5" and "a substring is not a match"; QEMU boot unchanged (release smoke 8/8) |
