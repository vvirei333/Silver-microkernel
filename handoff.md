# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## TABLET PORT (Lenovo Legion Y700, SM8650) — see tablet/HANDOFF.md for details

The tablet port (real hardware, not QEMU) lives in `tablet/` and is documented
authoritatively in **tablet/HANDOFF.md**. High-level status (2026-10-30):

- **Breakthrough**: the EFI/PE stub (`tablet/efi_stub.S` → `tablet/Image.efi`)
  now EXECUTES on the device via GRUB `chainloader`. First visible proof: the
  stub paints the splash framebuffer (0xD5100000) and a GREEN band appears.
- Current milestone: the stub reaches entry + kernel copy (green+white bands),
  but HANGS at its own MMU-disable step. Fix in progress (PC-relative MMU-off,
  or skip MMU-off and let the kernel `_start` handle it at its identity-mapped
  0xA8000000).
- Diagnostics: pmsg/ramoops readback is DEAD on this device; the ONLY channel is
  the GOP framebuffer (0xD5100000). QEMU regression is clean throughout.

## State as of 2026-10-29 (visible GUI clicks; "EL0" terminology)

Two polish items landed on top of the existing graphics/input work:

- **Button clicks are now visible in the console.** `gpu_driver` no longer
  relies on its own `syscall::print` (which the kernel drops because the driver
  does not own the console). On a button release it sends an IPC message to the
  shell, which owns the console and prints `GUI: Button '<name>' pressed!`.
  Howl registers itself as `"howl"` at startup; `gpu_driver` resolves that name
  lazily at the first click, so spawn order does not matter. The "Power" button
  still also calls `syscall::power_off()`. The message uses IPC opcode `2`
  (`GUI_BUTTON_PRESSED`), carrying the button label; `0`/`1` are already taken
  by Howler input and pointer samples respectively.
- **Terminology.** ARM64 has no "protection rings": user code runs at EL0 and
  the kernel at EL1. "Ring 3" was replaced with "EL0" (or "EL0 (userspace)")
  project-wide — README, handoff and source comments. Function/variable names
  had no "ring" to rename; the virtqueue "used/available/descriptor ring" names
  are the unrelated circular-buffer meaning and were left alone.

## State as of 2026-10-28 (power-off via PSCI; the GPU "Power" button shuts the machine down)

**Power-off is complete and verified in QEMU.** Clicking the "Power" button on
the engineer menu shuts the virtual machine down through PSCI `SYSTEM_OFF`.

### What changed

- `SYS_POWER_OFF = 71` (`sys_power_off`): issues PSCI 0.2 `SYSTEM_OFF`
  (`0x8400_0008`) with `x1..x3 = 0` via **`hvc #0`**. QEMU's `virt` machine
  boots the guest at EL2 and exposes PSCI through the **HVC** conduit, so an
  `smc` would trap as an EL1 sync exception and halt (`EL1 EXCEPTION kind=0`);
  `hvc` reaches the hypervisor's PSCI handler, QEMU prints "Power down" and
  exits. The call never returns. (The user's original spec said `smc #0`; HVC is
  the correct conduit for this machine.)
- `userspace/src/bin/gpu_driver.rs`: the click handler reports every release to
  the shell over IPC (see the 2026-10-29 section) and calls `syscall::power_off()`
  when the released button is "Power" (index 4 in `BUTTON_LABELS`). The kernel's
  `POWER OFF: PSCI SYSTEM_OFF` line is a raw PL011 write and always visible.

### Verification

Headless: QMP `input-send-event` moves the tablet pointer to the Power button
center (raw `(16409, 17512)` → pixel `(320, 256)`) and clicks. The serial shows
`POWER OFF: PSCI SYSTEM_OFF` and QEMU exits (no `PSCI returned`, no
`EL1 EXCEPTION`).

### Note for later

`SYS_POWER_OFF` is currently ungated: any task can request it and halt the whole
system. It is intentionally narrow (PSCI `SYSTEM_OFF` only — no `SYSTEM_RESET`,
no arguments), but a capability bit or a console-owner check is the natural
hardening once multi-user isolation matters.

## State as of 2026-10-28 (Phase 7 complete: EL0 fault containment + task teardown)

**Phase 7 (EL0 fault handling + teardown) is complete and verified in QEMU.** An
EL0 task — embedded or dynamically loaded — that faults (bad pointer,
privileged MSR, EL1-only MMIO) is killed cleanly and the system keeps running.

```text
howl> run bin/crash
crash: writing to 0x0...
Task 11 killed: Page Fault at 0x0000000000000000 (ESR: 0x0000000092000045)
howl> ps        # task 11 is gone; silverd/scout/howl/howler still listed
```

### What changed

- The sync-exception vector already split EL0/EL1 (`SPSR.M[3:2] == 0b00` →
  `silver_handle_task_fault`); this phase tightened the two handlers rather than
  rebuilding the teardown, which was already correct for dynamic tasks.
- `silver_handle_task_fault` now logs `Task <id> killed: Page Fault at 0x...
  (ESR: 0x...)` (task ID from `scheduler::current_task_id()`, plus `ELR_EL1` and
  the switch trace), then calls `scheduler::exit_current()`. Teardown marks the
  task Dead, closes its IPC/VFS, recovers the console and schedules away;
  `reap_dead_tasks` then releases the kernel and user stacks, the TTBR0 page
  tables and `image.release()` (the ELF frames) — the same path embedded and
  dynamic tasks share, so a crashed `run` program frees exactly its own pages.
- `silver_handle_exception` (the EL1 path) now **halts** in a `wfe` loop after
  dumping kind/ESR/ELR/FAR, instead of `exit_current()`. An EL1 exception is a
  kernel bug that cannot be survived; boot/allocator/IRQ failures reported
  through kind 4..8 use the same halt. This closes the long-standing "EL1 path
  is a stub" tech-debt item.
- `userspace/src/bin/crash.rs` writes to `0x0` via `write_volatile` (so the
  store survives release optimization) and is seeded at `/bin/crash` like
  `hello.elf`. `run bin/crash` faults with a data abort from EL0
  (`ESR 0x92000045`: EC 0x24, DFSC 0x05 translation fault), the kernel kills
  the task and the shell returns to the prompt.

### Verification

`run bin/crash` → `Task 11 killed: Page Fault at 0x0...` → `howl>` returns; `ps`
no longer lists task 11 and `run bin/hello` still prints `Hello from ELF!`. The
embedded boot chain is unchanged.

## State as of 2026-10-28 (Phase 6 complete: dynamic ELF loader)

**Phase 6 (Dynamic ELF loading) is complete and verified in QEMU.** The kernel
now loads an ELF64 file from the RAM-VFS into a fresh EL0 task, without the
binary being embedded at build time. The shell command `run <path>` reads the
file by path, the kernel parses and loads it, and the new task runs in EL0 with
its own address space, stack and W^X page permissions.

```text
howl> run bin/hello
Spawning ELF at 0x0000000040000000 with stack at 0x000000007FE00000
Hello from ELF!
```

### What changed

- `src/elf.rs` is the loader proper. `Elf64Ehdr`/`Elf64Phdr` parse the header and
  program table; `load_elf(bytes) -> Result<LoadedImage, ElfError>` allocates
  contiguous frames per `PT_LOAD` segment, copies file bytes, zero-fills BSS and
  returns an owned `LoadedImage { physical_start, page_count, entry, kinds }`.
  `PageKind` classifies every page as Text (R-X), ReadOnly (R--) or Data (RW),
  and `load_elf` enforces W^X by rejecting writable-executable and unreadable
  segments, overlapping permissions on one page, holes in the image and an entry
  point outside a text segment. `error_name` maps each `ElfError` to a short log
  label. `USER_IMAGE_VA = 0x4000_0000` and the 2 MB image window live here.
- `elf_loader.rs` is now a thin shim: `load(bytes)` delegates to `elf::load_elf`
  and logs the reason on rejection. There are no more parallel `UserImage` /
  `PageKind` types — `elf::LoadedImage` is the single image type everywhere.
- `mmu::create_task_space` and `scheduler::Task.user_image` take
  `elf::LoadedImage`; `Task::new` no longer loads ELF itself. The embedded path
  resolves the image in `spawn_inner` (before any frame is allocated, so a
  rejection needs no cleanup), and the dynamic path passes a pre-loaded image to
  `spawn_elf`.
- `spawn_elf(image, argument) -> Option<CapId>` mirrors `spawn_user_with_cap`:
  it allocates a kernel stack, the user stack and a private page, builds TTBR0,
  sets the entry from `image.entry`, pushes the task READY and returns a
  capability handle so the parent can lend the console, wait and kill. The new
  task receives **no** kernel-granted capabilities — device rights must be
  delegated by its parent. `spawn_elf_task` emits
  `Spawning ELF at 0x... with stack at 0x...` over the kernel UART.
- New syscall `SYS_SPAWN_PATH = 70` (`sys_spawn_path`): the kernel reads the file
  by path from the VFS (`vfs::read_file`), runs `elf::load_elf` and calls
  `spawn_elf`. The kernel reads the file itself because the ELF (tens of KiB) far
  exceeds the 4 KiB single-copy limit; the path is a small checked copyin.
  `EXEC_FILE_LIMIT` (4 MiB) bounds the blob before loading.
- `run <path>` in Howl (`run_elf`) calls `spawn_path`, lends the child the console
  via `console_set`, waits on `wait_event`, and reports exit/stop status exactly
  like `run_utility`; Ctrl-Z (Howler `stop_console`) returns control to the shell.
- `src/seed.rs` seeds `/bin/hello` into the VFS at boot (initramfs-style) from
  `include_bytes!(OUT_DIR/hello.elf)`. `build.rs` builds `hello.elf` alongside the
  other binaries but keeps it **out** of `user_program::image`, so the only way to
  run it is the dynamic loader. `seed::seed()` runs right after `vfs::init()`.
- `PROGRAM_DYNAMIC = u64::MAX - 1` is the sentinel program ID for dynamically
  loaded tasks (`ps` shows them as `elf`), distinct from `u64::MAX` (kernel tasks).

### Design decisions to preserve

- The image type is `elf::LoadedImage`, not a tuple: the scheduler needs
  `physical_start` and `page_count` to install TTBR0_EL1 and later free the
  frames.
- The user stack is reused as-is: `0x7FE0_0000`, 2 pages (8 KB). The image window
  stays `0x40000000..0x40200000` (2 MB). The dynamic loader introduces no second
  stack or image layout.
- Dynamic tasks get an empty capability table by default (secure), unlike
  embedded programs which receive self capabilities at spawn.

### Verification

`run bin/hello` from the `howl>` prompt prints `Spawning ELF at 0x0000000040000000
with stack at 0x000000007FE00000` and then `Hello from ELF!`. The embedded boot
chain is unchanged: all drivers and the shell start normally, and `ps` lists the
dynamic task as `elf`. Build is clean except the pre-existing `capability.rs`
warning.

### Limitations recorded for the next phase

- `run` reads the whole file into kernel memory before parsing it (bounded by
  `EXEC_FILE_LIMIT`), rather than streaming segments. Only static `ET_EXEC`
  images are supported; there is no PIE, dynamic linking, relocation or shared
  libraries.
- The dynamic task's argument is always `0`; there is no argv beyond the fixed
  numeric argument. `/bin/hello` is the only seeded file, and it is built by
  `build.rs` (not read from the on-disk SFS, whose server still exposes only the
  superblock).
- The user stack is fixed at 2 pages and the image at 2 MB, matching the embedded
  path; neither is configurable per program.

## State as of 2026-10-28 (Phase 5 complete: ramfb graphics + virtio-tablet input, live engineer menu)

**Phase 5 (Graphics + Input) is complete and verified in QEMU.** `gpu_driver`
paints a static "engineer menu" (BIOS / iPhone Field-Test style) onto a `ramfb`
framebuffer, and `input_driver` turns the `virtio-tablet-pci` mouse into a live
cursor that presses and releases the menu buttons.

Why `ramfb`, not PL111: the `virt` machine has no PL111 (that PrimeCell CLCD
exists only on realview/versatile/vexpress boards). `ramfb` is QEMU's native
plain-RAM framebuffer: the guest allocates a contiguous frame buffer, writes a
28-byte `RAMFBCfg` descriptor to the `etc/ramfb` `fw_cfg` file, and then paints
pixels; QEMU's display surface scans the RAM out.

### Current boot chain

`silverd` starts, in order:

1. Scout (bounded `SCOUT READY` handshake)
2. `block_driver` with `CAP_MMIO | CAP_PCI`
3. `net_driver` with `CAP_MMIO | CAP_PCI`, then lends it the console for 1 s
4. `sfs_server` with `CAP_MMIO`, then waits (bounded) for `SFS READY`
5. `net_server` with `CAP_MMIO`, passing the driver's task ID
6. `gpu_driver` with `CAP_MMIO`, then lends it the console for 1 s
7. `input_driver` with `CAP_PCI | CAP_MMIO`, then lends it the console for 1 s
8. Howl, then transfers the console
9. Howler with `CAP_MMIO | CAP_IRQ` and Howl's ID as its input target

`gpu_driver` registers the name `gpu_driver` before the input driver is spawned,
so `input_driver` resolves it with `SYS_LOOKUP` on its first pointer sample.

### Graphics driver (`userspace/src/bin/gpu_driver.rs`)

- Resolution 640x480, 32-bit ARGB. The frame buffer comes from `SYS_ALLOC_DMA`
  (Normal Non-Cacheable, `(vaddr, paddr)`); `SYS_MAP_PHYS` maps only the
  `fw_cfg` MMIO window, because `map_phys_to_user` rejects DRAM. The same split
  would hold for a real PL111: registers are MMIO, the scan-out buffer is RAM.
- `fw_cfg` MMIO is at `0x09020000` (`data` @+0x00, `ctl` @+0x08, `dma` @+0x10).
  Transfers use the DMA interface only: a 16-byte `FWCfgDmaAccess` struct is
  written to a scratch DMA page, then a single 8-byte store to the `dma`
  register (big-endian, so `value.swap_bytes()`) triggers the transfer. Byte
  stores to the `dma` register are rejected by QEMU and surface as
  `EL0 SYNC FAULT` (`ESR_EL1=0x92000050`, DFSC=0x10 external abort) — the
  register accepts only 4- or 8-byte stores.
- `find_ramfb_selector` scans the `fw_cfg` file directory (key `0x19`, 64-byte
  big-endian entries) for `etc/ramfb`; `configure_ramfb` writes the big-endian
  `RAMFBCfg { addr, fourcc=0x34325241, flags, width, height, stride }`.
- Drawing helpers: `clear_screen`, `draw_rect` (1 px border), `draw_text` /
  `draw_text_centered` over an embedded public-domain 8x8 font. The menu draws
  three grey status cells ("Num", "No Data", "Date"), a 3x2 grid of blue buttons
  ("Phone", "SMS", "Web", "Media", "Power", "Settings") and a bottom panel with
  "SN: 877..." and "Silver Microkernel v0.1".
- Live UI loop: `serve()` blocks on `SYS_RECEIVE`, decodes `InputMsg`, moves the
  cursor, tracks `pressed_btn`, and repaints the whole frame (clear + menu +
  cursor) after every report boundary. A held button fills dark blue
  (`0x003366`); releasing restores the normal blue and prints `Button clicked!`.
- `draw_cursor` draws an 8x8 white arrow last, on top of the menu.

### Input driver (`userspace/src/bin/input_driver.rs`)

- PCI scan matches vendor `1af4` device `0x1052` (`VIRTIO_ID_INPUT`, modern-only;
  there is no transitional virtio-input). Its 64 KiB BAR window is assigned at
  `0x1030_0000` (after the block `0x1010_0000` and net `0x1020_0000` windows).
- Single event queue (queue 0). QEMU creates a 64-entry queue; the driver
  provides 64 buffers of 8 bytes each (one `virtio_input_event =
  { type: u16, code: u16, value: u32 }` per buffer, all little-endian).
- QEMU's `virtio_input_send` queues events and flushes one batch per
  `EV_SYN`/`SYN_REPORT`, popping one available buffer per event and firing an
  IRQ. The driver polls the used ring every 10 ms (`sleep(1)`), so no virtio
  IRQ is wired.
- Event handling: `EV_ABS` code 0x00/0x01 updates raw X/Y, `EV_KEY` code
  `0x110` (`BTN_LEFT`) updates the click state, `EV_SYN` code 0 forwards one
  sample. Raw absolute values `0..=32767` are scaled to `0..=639`/`0..=479`.
- Each `EV_SYN` sends one `InputMsg` to `gpu_driver`, resolved once via
  `SYS_LOOKUP("gpu_driver")` and cached; `SYS_TRY_SEND` drops samples if the
  mailbox is full rather than blocking the poll loop.

### Shared protocol (`userspace/src/input_proto.rs`)

`InputMsg { x: u16, y: u16, left_clicked: bool }` packed into an `OP_POINTER` (1)
IPC message (`x`, `y` little-endian, `left_clicked` in `payload[5]`). A sample is
small enough for the 32-byte mailbox, so it travels over IPC directly — no DMA
mailbox is needed (unlike `net_ipc`).

### QEMU flags

`build_and_qemu.sh` replaced `-nographic` with:

```text
-device ramfb
-device virtio-tablet-pci,disable-legacy=on,id=tablet0
-display gtk,show-cursor=on
-serial mon:stdio
```

### Verification

Boot log shows `GPU DRIVER: ENGINEER MENU DRAWN` and `VIRTIO-INPUT READY`.
Headless verification drives the pipeline with QMP `input-send-event` (each
command ends in an implicit `EV_SYN`; there is no explicit `sync` event type),
then `screendump` plus pixel sampling confirmed: the cursor moves to the target
and leaves no trail; pressing over "Phone" fills it `(0,51,102)`; releasing
restores `(0,90,181)` while "SMS" stays normal.

### Limitations recorded for the next phase

- The menu is single-buffered: every sample repaints the whole 640x480 frame
  (clear + menu + cursor). There is no backbuffer/flip, so `render()` is the
  `present()`.
- Input is polled at ~100 Hz, not IRQ-driven; virtio-input MSI-X/INTx is not
  wired. Absolute scaling assumes QEMU's `0..32767` range (the device-config
  `ABS_INFO` min/max is not read).
- The input driver's BAR is fixed at `0x1030_0000` (assumes block `0x1010_0000`
  and net `0x1020_0000`).

## State as of 2026-10-28 (Phase 4 complete: VirtIO-Net, smoltcp, shared-memory IPC and shell ping)

**Phase 4 (Network) is complete and verified end-to-end in QEMU.** The shell
drives the stack:

```text
howl> ping 8.8.8.8
PING 8.8.8.8: OK
```

That line appears only after the whole path succeeds: `net_driver` finds and
initializes VirtIO-Net; `net_server` allocates an 8 KiB DMA window and announces
it; the driver maps the window and returns the NIC MAC; Howl parses `8.8.8.8`
with the strict dotted-quad parser and sends `OP_PING`; smoltcp resolves the
slirp gateway with ARP; the echo request crosses the TX mailbox and the TX
virtqueue; the reply crosses the RX virtqueue and the RX mailbox; and smoltcp
accepts it with a valid IPv4/ICMP checksum. `net_server` owns no console, so it
relays `OP_PING_REPLY` to Howl, which prints the readable line.

`net_driver` (EL0) owns the VirtIO-Net PCI function; `net_server` (EL0)
runs smoltcp. The two exchange only doorbells, and frame bytes travel through
one shared DMA mailbox (see below). The shell reaches the stack over ordinary
IPC with `OP_PING` / `OP_PING_REPLY`.

### Fixes landed in this phase

- **PCI scan no longer drops transitional virtio-blk.** `find_virtio_blk()`
  matched `1af4:1001` but discarded it whenever BAR0 was an I/O BAR. BAR0 no
  longer gates the match; it is mapped lazily only when a modern capability
  actually references it. (Full diagnosis in the 2026-10-27 section below.)
- **`ping` timeout race.** The shell waits 3 s (`PING_WAIT_TICKS`) while
  `net_server` times out its own wait at 2 s (`PING_TIMEOUT_MILLIS`), so the
  server's timeout reply always has a full second to reach the shell before it
  gives up.
- **Strict IPv4 parser.** `parse_ipv4` accepts exactly a dotted quad of four
  decimal octets 0–255 (no spaces, at most three digits each); anything else is
  rejected with a usage message.

### Current boot chain

`silverd` starts, in order:

1. Scout (bounded `SCOUT READY` handshake)
2. `block_driver` with `CAP_MMIO | CAP_PCI`
3. `net_driver` with `CAP_MMIO | CAP_PCI`, then lends it the console for 1 s
4. `sfs_server` with `CAP_MMIO`, then waits (bounded) for `SFS READY`
5. `net_server` with `CAP_MMIO`, passing the driver's task ID
6. Howl, then transfers the console
7. Howler with `CAP_MMIO | CAP_IRQ` and Howl's ID as its input target

Task IDs are runtime values, never API. The network server resolves the driver
from the task list (preferring the spawn hint only while it is live). The pair
is restart-tolerant: a restarted `net_server` re-attaches, and a restarted
`net_driver` is rediscovered by the server.

### VirtIO-Net driver (`userspace/src/bin/net_driver.rs`)

- PCI scan accepts vendor `1af4` with device `0x1000` (transitional) or `0x1041`
  (modern-only); both are driven through modern common/notify capabilities. The
  legacy I/O BAR is never used.
- Its 64 KiB BAR window is assigned at `0x1020_0000`; the block driver keeps
  `0x1010_0000`.
- Split virtqueues: RX is queue 0, TX is queue 1, with 16 descriptors of 2 KiB
  per queue. Every frame carries the 12-byte modern virtio-net header; the
  driver sends a zeroed header and skips it on receive.
- There is no virtio-net interrupt. `serve()` calls
  `receive_timeout(..., IDLE_POLL_TICKS = 1)` (10 ms), so IPC wakes it early and
  otherwise it polls `receive_packet()` once per tick. This is polling, not
  IRQ-driven networking.
- `OP_ATTACH`: reject windows smaller than `DMA_SIZE`; map with `SYS_MAP_PHYS`;
  store `{vaddr, paddr, size, server}`. Re-announcing the same window keeps the
  mapping and refreshes the server task ID; it is not mapped twice. Reply with
  `OP_ATTACHED` and the MAC.
- `OP_TX`: accept only from the recorded server task ID; read `TX_LENGTH`; copy
  at most `MAX_FRAME` bytes into a static buffer; clear `TX_LENGTH` *before*
  `send_packet`, so the server may publish the next frame without racing the
  virtqueue.
- RX: only when `RX_LENGTH == 0`; copy one frame into `RX_FRAME`, `dmb ish`,
  publish the length and send `OP_FRAME`.
- RX and TX copies use static frame buffers because the userspace stack is only
  8 KiB.

### Shared-memory frame protocol (`userspace/src/net_ipc.rs`)

`net_server` allocates `DMA_SIZE = 8192` through `SYS_ALLOC_DMA`; the kernel maps
it Normal Non-Cacheable in both address spaces. IPC carries only control
messages and doorbells; an Ethernet frame never enters the 32-byte mailbox.

| Offset | Contents |
| --- | --- |
| `0x000` | `u32` RX length, written by the driver |
| `0x004` | `u32` TX length, written by the server |
| `0x800` | `MAX_FRAME` (1536) bytes, driver -> server |
| `0x1000` | `MAX_FRAME` (1536) bytes, server -> driver |

| Opcode | Direction | Meaning |
| --- | --- | --- |
| `OP_ATTACH` (1) | server -> driver | DMA physical address and size |
| `OP_ATTACHED` (2) | driver -> server | window mapped; NIC MAC follows |
| `OP_FRAME` (3) | driver -> server | a frame is in the RX slot |
| `OP_TX` (4) | server -> driver | a frame is in the TX slot |
| `OP_PING` (5) | shell -> server | one ICMP echo request; `payload[1..5]` is the IPv4 address |
| `OP_PING_REPLY` (6) | server -> shell | `payload[1]` is success, `payload[2..6]` echoes the address |

Publication rule: payload stores, then `dmb ish`, then the length store. The
consumer loads the length, `dmb ish`, copies the frame, then clears the length.
There is exactly one slot per direction. A producer publishes only when the
length word is zero, so a slow consumer applies backpressure instead of
overwriting a frame. TX doorbells from any task other than the attached server
are ignored; an oversized or zero TX length is reset and dropped.

### smoltcp server (`userspace/src/bin/net_server.rs`)

- `no_std`/`no_alloc`, with `medium-ethernet`, `proto-ipv4`, `socket-icmp` and
  `socket-udp`. A static `FrameDevice` holds one maximum-size (1536-byte) frame
  buffer per direction; the interface plan is IPv4 only.
- Starts with QEMU's default MAC, `10.0.2.15/24` and default route `10.0.2.2`.
  `OP_ATTACHED` replaces the MAC with the real one read by the driver. The MAC
  must not be hardcoded: ARP would otherwise use the wrong source address.
- Allocates and zeroes both length words before the first attach, and retries
  `OP_ATTACH` once a second until `OP_ATTACHED`. It no longer pings at boot:
  the ICMP socket (identifier `0x5349`) is idle until the shell sends `OP_PING`.
- On `OP_PING`, the server queues one ICMPv4 echo request to the given address.
  smoltcp performs ARP first and keeps the echo queued in the socket until the
  neighbor resolves. Only one echo is pending at a time: the ICMP socket is
  identified, so every pending request would match every reply.
- Main loop: poll the RX length word, hand the frame to smoltcp, clear the word,
  run `interface.poll`, drain ICMP replies, publish at most one TX frame when
  the slot is free, then sleep 10 ms. The loop also drains IPC doorbells so the
  driver's mailbox cannot fill up; frame delivery does not depend on a doorbell
  arriving.
- A pending echo that outlives its two-second window (`PING_TIMEOUT_MILLIS =
  2000`) is answered as a timeout. The result is reported to Howl as
  `OP_PING_REPLY` (success bit + the echoed address); the shell prints
  `PING <ip>: OK` or `PING <ip>: TIMEOUT`. Arbitrary network/server diagnostics
  are still discarded because `net_server` owns no console.
- A missing reply is never printed as success: the server answers a timeout
  itself, and the shell additionally checks that the echoed address matches.

### Verification

```bash
./build_and_qemu.sh release
```

Expected observable result at the `howl>` prompt:

```text
howl> ping 8.8.8.8
PING 8.8.8.8: OK
```

`ps` shows live `net_driver` and `net_server` tasks. `build_and_qemu.sh` does not
pass an explicit NIC; the `virt` machine's implicit default `virtio-net-pci`
provides QEMU user-mode networking with guest `10.0.2.15`, gateway `10.0.2.2`.
If the NIC were absent, the driver would exit with a stage-coded status and
`net_server` would keep retrying the attach.

### Network limitations recorded for the next phase

- IPv4 only, static address/gateway; no DHCP, DNS, IPv6 or TCP/UDP services.
- One frame slot per direction; no zero-copy, scatter-gather or multi-queue.
- RX is polled, not interrupt-driven; latency and throughput are bounded by the
  10 ms loop. No virtio-net IRQ is wired.
- `ping` is one-shot and single-flight: one ICMP echo request at a time, no
  timing statistics or continuous mode. There is no detach/unmap protocol: if
  `net_server` dies, the driver keeps the old mapping until a new attach.

## State as of 2026-10-27 late evening (BAR0 fix: transitional virtio-blk; SFS mounts end-to-end)

**The `driver_status=258` mystery is solved and SFS mounts.** A release boot
now prints `SFS MOUNTED`, `sfsinfo` reports magic `0x53465321` and `ls` lists
`bin/ etc/ tmp/` — the whole IPC/DMA path works end-to-end.

Root cause of `258` (`payload[2, 1]` = NOT_INITIALIZED, stage NOT_FOUND):
QEMU 11's default `virtio-blk-pci` is a **transitional** device. It reports
device id `0x1001` (so the vendor/device match was always correct) but routes
the legacy region into **BAR0 as an I/O BAR** (bit 0 = 1) and keeps the modern
common/notify capabilities in BAR4. `find_virtio_blk()` read BAR0 and did
`if bar0 & 0x1 != 0 { continue; }` — discarding the device it had just
matched, even though BAR0 never participates in the capability-driven setup.
The scan therefore "found nothing" → stage 1.

Diagnosis path worth repeating (driver prints are discarded without the
console):

- `qemu-system-aarch64 ... -monitor unix:/tmp/qmon,server,nowait` and
  `info pci` shows vendor/device ids and BAR types directly (`BAR0: I/O`,
  `BAR4: 64 bit prefetchable`). It also revealed QEMU's implicit default
  `virtio-net-pci` (`1af4:1000`) on the bus, which the driver correctly skips.
- The ECAM hardcode is **not** a bug: the QEMU virt DTB says
  `reg = <0x40 0x10000000 0 0x10000000>` (base `0x4010000000`, 256 MB, buses
  0–255), exactly matching `mmu::PCI_ECAM_BASE/SIZE`.

Fix applied in `userspace/src/bin/block_driver.rs`: BAR0 no longer gates the
match. `ASSIGNED_BAR0` and `assign_bar0()` are gone; BAR0 is mapped lazily via
a new `bar0_base()` helper (rejects I/O BARs) only when a capability actually
references BAR0 (modern-only devices), and the `_start` print now reports the
PCI BDF instead of "AT BAR0".

**Second, separate issue found by the fix:** with the driver alive, mount
still failed with `code=7` (`STAGE_BAD_MAGIC`, `reads=20`) — because
`disk.img` was not SFS-formatted at all (sector 0 started with the `dead beef`
pattern). **`disk.img` must be formatted once with `python3 format_sfs.py`**
after `run_disk.bat` or any other reset of the image; `run_disk.bat`
recreates it with the `DEADBEEF` pattern, which is not a superblock. Both
images were identical `dead beef` at the time of writing, so no data was
lost; formatting is safe and `disk.img` is git-tracked.

Also in this session: `build_and_qemu.sh` verification requires killing stale
QEMU processes (a running instance holds the `write` lock on `disk.img`), and
this environment's `pkill` is shadowed by an AppImage wrapper — use
`kill -9 <pid>` with a PID from `fuser disk.img` instead.

## State as of 2026-10-27 evening (stage-code exit diagnostics; driver init under suspicion)

`sfs_server` loads fine through the ELF loader but exits during mount. A
release boot printed:

```text
EXIT: task=7 program=11 status=8589939716 (0x000000020001404)
```

Decoded: stage 4 (`STAGE_READ_REJECTED`), attempts 20, reads 0,
`last_status` 2. **That 2 is NOT `VIRTIO_BLK_S_IOERR`.** In the block
driver's reply protocol `payload[0] == 2` means *queue not initialized*
(`QUEUE == None`): the driver answers from its IPC loop before any virtio
request is ever submitted, so QEMU never saw the request and no device-level
I/O error happened. The virtio descriptor chain in `read_sector`, the u64
sector encoding (`virtio_blk_outhdr` at offset 8, IPC wire
`payload[0]=opcode, payload[1..9]=sector`) and the volatile buffer zeroing
(separate 4 KB `alloc_dma` page; the virtqueue is a separate block) were all
audited and are correct — and unreachable until the queue exists.

Changes made to find the failing driver init step:

- `userspace/src/bin/sfs_server.rs`: mount failures now exit with
  stage-coded statuses. `status & 0xFF` names where mount gave up
  (`1..=7`: NO_DRIVER, SEND_FAILED, NO_REPLY, READ_REJECTED, SHORT_WINDOW,
  MAP_FAILED, BAD_MAGIC); bits 8-15 attempts, 16-23 reads, 32-47 the last
  driver status word. On a reply of 2 the status word packs the driver's
  init failure stage into its high byte.
- `userspace/src/bin/block_driver.rs`: `INIT_FAILURE` plus init stage codes
  (1 NOT_FOUND, 2 BAD_VERSION, 3 FEATURES, 4 NO_QUEUE, 5 QUEUE_SETUP); the
  "not initialized" reply is now `[2, stage]` so the client's exit report
  pinpoints the driver step. The spec-violating
  `notify_off_multiplier == 0 → None` rejection in `prepare_virtqueue` was
  removed: a zero multiplier is legal (all queues notify through one
  register; the written queue index selects the queue) and the notify
  address formula already handles it. This guard is the prime suspect for
  the queue never appearing on some QEMU builds.
- `build_and_qemu.sh` (new): builds the kernel + embedded userspace ELFs
  (release by default, `debug` arg) and boots QEMU with `disk.img` attached
  as `virtio-blk-pci`. It also prepends rustup's toolchain bin dir to PATH
  when cargo is not reachable (IDE sandbox shells). `--no-run` builds only.
  **A QEMU invocation without `-device virtio-blk-pci` produces the exact
  same EXIT signature as a driver init failure** — always boot with the
  disk attached.

Next step: boot via `./build_and_qemu.sh` and read the new EXIT status —
bits 40-47 name the driver init stage (1 = no virtio-blk device in the
command, 3 = feature negotiation, 5 = queue setup/`alloc_dma`), or `SFS
MOUNTED` if the multiplier fix was the whole story.

## State as of 2026-10-27 (SFS mounts; `sfsinfo` still mismatched)

**The SFS superblock now mounts.** A release boot shows neither
`SILVERD: SFS MOUNT FAILED` nor `SILVERD: SFS SERVER NOT READY, CONTINUING`,
which means `await_sfs_ready` received `SFS READY` and `sfs_server` entered
`serve()`. The client reads the DMA window correctly (magic `0x53465321`
matched), so the "client sees zeroes / wrong sector" symptom is gone.

Four independent defects had to be fixed to get there; only the last one is
still open for `sfsinfo`.

1. **The DMA window was zeroed *after* the device wrote it.**
   `read_sector` cleared the 4 KB request buffer with
   `core::ptr::write_bytes`, a non-volatile store. LLVM is allowed to sink it
   past the volatile completion polling, and in optimized builds it did exactly
   that: the sector the device had just transferred was wiped before the client
   mapped the window. Debug builds looked fine, release builds returned zeroes.
   It now clears with `write_volatile` in 8-byte units followed by
   `compiler_fence(SeqCst)`, and `read_superblock` fences after its volatile
   loads so the magic check cannot be hoisted above them.
2. **`MAIR_EL1` selected the wrong attribute bytes.** `MAIR_VALUE` was
   `0x00_44_FF`, i.e. index 1 (`ATTR_DEVICE`, used for MMIO) resolved to `0x44`
   Normal Non-Cacheable and index 2 (`ATTR_NORMAL_NC`, used for the DMA window)
   resolved to `0x00` Device-nGnRnE — the two attributes were swapped. The value
   is now `0x44_00_FF`: index 0 `0xFF` Normal WB, index 1 `0x00` Device-nGnRnE,
   index 2 `0x44` Normal Non-Cacheable.
3. **Stale translations on remap.** `map_user_pages` relied on one global
   flush after installing L3 descriptors. It now cleans the L3 line
   (`clean_table_lines`, `dc cvac` + `dsb ishst`) and then invalidates each page
   by address (`invalidate_user_page`, `tlbi vaae1is`) instead of a single
   `flush_after_table_change`. The address form keeps the rest of the TLB warm
   and cannot be silently skipped when a page table was just installed.
4. **Block-driver request layout disagreed with the client.** The client put
   the sector at `payload[0..8]` and the driver read `payload[..8]`, while every
   other option is prefixed with an opcode. The wire format is now explicit on
   both sides: `payload[0] = OP_READ_SECTOR` (`= 1`),
   `payload[1..9] = sector.to_le_bytes()`. Reply unchanged: `payload[0] = 1` on
   success, `payload[1..9]` = DMA window physical address, `payload[9..11]` = 512.


### Exception path: EL0 fault diagnostics are Rust again

The EL0 sync vector had `pl011::write_str` / `write_hex_u64` calls spliced into
its `global_asm!` block. Rust calls cannot appear in an assembly template, so
that block could never assemble (the strings would have been fed to the
assembler as instructions). The dump now lives in `silver_handle_task_fault`,
which already receives `esr` / `elr` / `far` in x0-x2 per AAPCS64 from a
four-instruction vector body:

```asm
exception_user_fault:
    mrs x0, esr_el1
    mrs x1, elr_el1
    mrs x2, far_el1
    b silver_handle_task_fault
```

A fault prints one block and then kills only the guilty task:

```text
EL0 SYNC FAULT!
ESR_EL1=000000009200000E
ELR_EL1=0000000040005AF0
FAR_EL1=FFFF000040000000
SWITCH TRACE from,to,sp,elr:
15->1 sp=FFFF000040740000 elr=FFFF00004009037C
```

`scheduler::dump_switch_trace()` is called again before `exit_current()`, so
the 16-entry ring is no longer dead code. Labels use fixed-width uppercase hex
and `\r\n`, like every other kernel dump.

### Open issue: `sfsinfo` gets opcode 5 instead of 0

`SFS NOT MOUNTED` is printed when the reply's `payload[0] != 1`. A probe that
dumps all 32 reply bytes returns, on a release run:

```
howl> sfsinfo
SFS NOT MOUNTED
  +0  0x0000000000050500
  +8  0x0000000000000000
  +16 0x0000000000000000
  +24 0x0000000000000000
```

The server-side probe fills `payload[1] = request.payload[0]`,
`payload[2] = request.sender as u8`, `payload[3] = OP_INFO`, so the reply decodes
as `payload[0] = 0` (status: not mounted), `payload[1] = 5`, `payload[2] = 5`,
`payload[3] = 0`. The server therefore saw `request.sender == 5` (correct: the
shell is task 5) but `request.payload[0] == 5` — the sender ID, where the shell
had written `0`.

**Fix applied:** In `userspace/src/bin/silvershell.rs`, `sfs_info()` now explicitly
sets `request_payload[0] = 0` before sending the request, so the server can
properly set `payload[0] = 1` (mounted) in its reply without echoing the sender ID.

---

## Open issue: Block driver DMA allocation (reads=0 in Release)

After fixing the `sfsinfo` 8-byte shift, the `sfsinfo` command now prints
`SFS SERVER NOT RUNNING`. In the loading log, `reads=0` is observed, meaning
`read_sector` returns `false` before any sector read is attempted.

**Root cause:** `syscall::alloc_dma(4096)` in the block driver returns 0 in
Release mode. This triggers the `if request_paddr == 0 { return false; }` check
added to prevent writing to the zero DMA page. The DMA allocation failure may be
due to:
- Frame allocator not properly initialized (all pages marked used, none free)
- Task lacking `CAP_MMIO` rights required for DMA allocations
- No contiguous physical memory available for DMA

**Workaround:** The block driver now checks if `REQUEST_BUFFER` was successfully
allocated and handles the failure gracefully by returning `false` from `read_sector()`
with a diagnostic message. The driver will not be able to read sectors until DMA
allocation is resolved.

**To debug:** Run the kernel in QEMU with debug output to determine whether the
frame allocator has free pages, whether the block driver task has `CAP_MMIO`, and
whether `alloc_dma_for_current` succeeds or fails and why.

---

Four independent defects had to be fixed to get SFS mounting; the DMA allocation
issue is the remaining open problem for block device support. { sender: u64, payload: [u8; 32] }` — identical.
- `sys_send` copies `size_of::<Message>()` bytes from the user pointer and then
  overwrites `sender` with the current task ID; `sys_receive` copies the same
  40 bytes back. `user_memory::copy_from_user` / `copy_to_user` are byte-wise
  with `+ index` addressing, so neither can shift the payload.
- The reply direction has no shift: the shell's dump is byte-for-byte what the
  server's probe wrote.
- Task table on this build (`ps`): `0 kernel`, `1 silverd`, `2 scout`,
  `3 block_driver`, `4 sfs_server`, `5 howl`, `6 howler`. `sfs_server` is task 4
  and the reply really comes from it (`receive_from` filters on
  `buffer.sender == task_id`).

The decisive experiment is staged in the working tree but **has not been run**:
the shell sends `[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, ...]` and the
server echoes `request.payload[..8]` into the reply's `payload[8..16]`. The `+8`
word of the dump gives the shift directly:

- `0x0000776655443322` — the server received exactly what was sent; then the bug
  is specific to the one-byte `&[0]` request path, not to marshalling.
- zeroes, or a variant with the bytes shifted by one — a real shift; keep the
  `payload[0] = opcode` convention and move the affected field accordingly.
- a word containing `0x05` — the sender ID is being substituted for
  `payload[0]` somewhere between `sys_send` and `serve()`.

Until that is settled `sfsinfo` reports `SFS NOT MOUNTED` even though the
filesystem itself is mounted.

### Verifying a boot (harness)

`run_disk.bat` must **not** be used for SFS work: it recreates `disk.img` with a
`DEADBEEF` pattern and boots the debug kernel. Use the Python harness instead —
it attaches the existing `disk.img`, waits for the `howl>` prompt and sends the
command terminated with `\r\n` (a bare `\r` never executes):

```powershell
Get-Process qemu-system-aarch64 -ErrorAction SilentlyContinue | Stop-Process -Force
cd 'F:\My Projects\Silver microkernel'
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std
python 'C:\Users\Public\sfs_check.py' 'target/aarch64-unknown-none/release/silver_microkernel' 'sfsinfo' 12
```

A hanging QEMU keeps `disk.img` locked, so always kill it first.
`C:\Users\Public\sfs_check.py` takes `<kernel-elf> <command> <seconds>`.

Note for any new probe: `syscall::print` from a task that does not own the
console silently returns 0, so `block_driver` and `sfs_server` cannot print
anything. Probes must travel back inside the IPC reply payload.

### Temporary probes in the working tree (remove before committing)

- `userspace/src/bin/block_driver.rs`: a `BLOCK SECTOR 0x…` `syscall::print`
  (invisible, harmless); the reply payload is **27 bytes** instead of 11, with
  `payload[11..15]` = first four bytes the driver reads back (`data[..4]`) and
  `payload[15]` = the sector it parsed.
- `userspace/src/bin/sfs_server.rs`: `MountFailure.driver_view` /
  `driver_sector` and their use in `announce` (`payload[26..30]`, `payload[30]`
  replacing the DMA physical address), plus the `serve()` else branch that
  echoes `request.payload[0]`, `request.sender` and `OP_INFO` and copies
  `request.payload[..8]` into `payload[8..16]`. Restore the else branch to
  `ipc::Message::new(0, &[0])` and the 11-byte success payload.
- `userspace/src/bin/silverd.rs`: prints `DRIVER 0x… SECTOR 0x…` from
  `payload[26..30]` / `payload[30]`; restore the `PADDR` print from
  `payload[26..32]` once `driver_view` / `driver_sector` are gone.
- `userspace/src/bin/silvershell.rs`: the patterned request payload and the
  32-byte dump loop in `sfs_info`'s `SFS NOT MOUNTED` branch. Restore
  `ipc::Message::new(0, &[0])` and the single-line message.

Changes in these files that must stay: the volatile zeroing plus fence in
`block_driver::read_sector`, decoding the sector from `payload[1..9]`, the
`OP_READ_SECTOR` request header in `sfs_server::mount`, the fence in
`read_superblock`, and the four `mmu.rs` fixes above.


## State as of 2026-10-26 (DMA/PCI infrastructure and EL0 UART; historical)

> Boot order, SFS and network status in this section are historical. The
> 2026-10-28 section above is the current boot chain.

Architecture at that point: **silverd -> Scout, block_driver, Howl, Howler**.
Normal boot creates only silverd (task 1). Silverd spawns Scout, waits for its
`SCOUT READY` IPC handshake (bounded to 2 s, boot continues on timeout), spawns
block_driver with CAP_MMIO|CAP_PCI, then spawns Howl and transfers the console,
and finally spawns Howler with Howl's ID as its input target. With the current
spawn order that is Scout=2, block_driver=3, Howl=4, Howler=5, but **no
component depends on those numbers**: every cross-task reference is resolved at
runtime (see below). The block driver is a full Virtio-PCI block driver (modern
common/notify capabilities, split virtqueue, descriptor chain, `read_sector`,
shared DMA reply buffer) but it does not implement SFS; the filesystem server is
not started at boot.
Howler maps PL011 and binds INTID 33. Normal user RX and TX both pass through
Howler in EL0 (userspace): input messages are `[0, byte]`, while output messages are
`[1, length, up to 30 bytes]`. `SYS_PRINT`, `SYS_PRINT_HEX` and `SYS_PRINT_DEC`
chunk console output into nonblocking IPC messages to the current console sink.
The sink is resolved at runtime by `scheduler::console_sink_task()`: the live task
whose `bound_irq` is INTID 33. Task IDs must never be hardcoded here — a spawned
task that happens to reuse the old ID (e.g. `spawn block_driver` taking ID 4)
would otherwise swallow the console output into an undrained mailbox. The kernel
wakes the sink's event wait for TX as well as hardware RX and yields after each
chunk. Early diagnostics, `demo-tasks` and a dead-Howler fallback still use kernel
PL011; a full TX mailbox drops the remaining output instead of blocking.

Userspace follows the same rule. `silvershell` resolves the block driver with
`find_task_id(PROGRAM_BLOCK_DRIVER)` over `SYS_TASK_LIST`, skipping dead and
stopped tasks, and resolves Howler the same way (`HowlerLink`, refreshed at most
once per second) instead of trusting the boot argument on liveness alone. Both
resolutions prefer the **most recently spawned** matching task: a respawned
driver re-runs device setup and therefore owns the hardware, while an older
instance keeps a virtqueue the device no longer uses. The driver itself accepts
requests from any caller and answers `request.sender` from the kernel-built
header, and it abandons a client that neither acknowledges nor stays alive within
10 s, so a crashed client cannot wedge the device.

The QEMU virt DTB describes the PL011 interrupt as SPI 1, level-high, which is
GIC INTID 33. The kernel fallback uses PL011 RXIM and RTIM with a bounded
256-byte FIFO; its IRQ handler drains the hardware FIFO completely. `SYS_READ`
reads only that software queue and never polls hardware. The timer no longer
polls UART. Both the unbound fallback and Howler consume Ctrl-Z (`0x1A`) as a
stop request rather than forwarding it to Howl.

Keep `GICD_ICFGR = 0b00` for this level-sensitive interrupt. `0b10` means
edge-triggered and left input stuck during development. RX timeout (RTIM) is
required so a single keypress does not wait for the RX FIFO threshold.

The interactive profile still has one UART owner. `spawn` and `bg` create children
without it; `fg <cap>` resumes a stopped task or foregrounds a live one, then waits
for either exit or stop. Ctrl-Z changes the child to `Stopped`, returns the console
to the shell and wakes the foreground waiter. Howler invokes `SYS_STOP_CONSOLE =
66`; the kernel stops the current console owner and schedules if one was stopped.
`ps` reports state code 7 as `stopped`. `SYS_TASK_RESUME = 40` and
`SYS_WAIT_EVENT = 41` implement resume and the `Exited`/`Stopped` event split;
old `SYS_WAIT` still waits only for exit.

A stopped task keeps `resume_state`: `Running` becomes `Ready`, while sleep and
blocked waits retain their deadlines/queue membership. Expiry or notification
while stopped updates the saved state instead of accidentally scheduling the task.
Killing stopped/blocked IPC receivers wakes both `not_empty` and `not_full` before
mailbox destruction, preventing a dangling WaitQueue during reaping.

Focused security review found and fixed four concrete issues: `SYS_SEND` now
overwrites the user-provided `Message.sender` with the current task ID; mutex
unlock rejects non-owners in release as well as debug; nameserver decimal ID
parsing uses checked arithmetic; and sleep deadlines use saturating addition.
Regression role 27 verifies sender stamping, non-owner/owner unlock and survival
of a child sleeping for `u64::MAX` milliseconds. This was a focused review, not a
complete security audit. Task/memory/wait-queue/file resource quotas remain open.

Capability model implemented. Raw Task IDs are no longer sufficient for kill/wait/console.
`SYS_CAP_GRANT = 61` delegates a subset of an existing capability's rights to
another live task; recipients cannot receive rights the caller does not hold.
When a target dies, its capabilities lose every right except `CAP_WAIT`; that
wait-only capability is revoked when the completion record is consumed.
`SYS_MAP_PHYS = 62` requires `CAP_MMIO` and maps non-DRAM physical pages as
Device-nGnRnE into a first-fit 2 MB user window at `0x80000000`.
`SYS_BIND_IRQ = 63` requires `CAP_IRQ` and exclusively binds a non-timer INTID.
`SYS_WAIT_IRQ = 64` consumes a pending notification or blocks until one arrives.
`SYS_TRY_SEND = 65` rejects a full or dead mailbox without blocking.
`SYS_STOP_CONSOLE = 66` lets Howler deliver Ctrl-Z without forwarding the byte.
Bound level-triggered IRQs are masked before GIC EOI, then unmasked by
`bind_irq` and every `wait_irq` call to prevent an IRQ storm while EL0 handles
the device. Program 4 (Howler) receives self capabilities with CAP_MMIO and
CAP_IRQ at spawn, plus an explicit CAP_GRANT from silverd. It maps PL011, binds
INTID 33, sends input to Howl and writes output IPC to DR without entering the
kernel PL011 handler on the normal path.

`SYS_ALLOC_DMA = 67` requires CAP_MMIO, accepts a byte size in x0 and returns a
page-aligned user VA in x0 plus its physical DMA address in x1. Allocations are
zeroed contiguous frames, rounded to whole pages and limited to 2 MB total per
call. They share the first-fit mapping window at `0x80000000` with MMIO, never
the heap, and are mapped Normal Non-Cacheable (MAIR index 2, value `0x44`) rather
than Device memory. The kernel cleans the cacheable aliases before exposing the
buffer and releases tracked frames during task reaping. No DMA-free syscall or
per-allocation quota exists yet.

`SYS_PCI_READ = 68` and `SYS_PCI_WRITE = 69` require CAP_PCI. Their offset is a
4-byte-aligned byte offset within QEMU virt's 256 MB ECAM at physical
`0x4010000000`; reads/writes are volatile 32-bit accesses through a kernel
Device-nGnRnE mapping. Program 10 (block_driver) receives self capabilities with
CAP_MMIO and CAP_PCI at spawn, plus an explicit CAP_GRANT from silverd. It walks
the ECAM bus/device/function space for vendor `0x1AF4` with device `0x1001` or
`0x1042`, maps BAR0/BAR4, negotiates VIRTIO_F_VERSION_1, allocates the split
virtqueue (descriptor table, available and used rings in one DMA allocation),
posts a `{type=0, sector}` request with inline 512-byte data and a status byte,
and replies over IPC with the shared DMA physical address plus length. The client
maps that buffer and releases it with a `0xA5` acknowledgement.

Verified in QEMU debug and release with `-device virtio-blk-pci`: `ps` showed
`scout`, `block_driver`, `howl`, `howler` as live tasks, `diskread` printed
`DISK SECTOR 0: 0xDEADBEEF...` and `hexdump 0` dumped the sector read from
`disk.img`. `spawn block_driver` created a second instance (ID 6) that re-ran
device setup; `diskread` then used it, `kill` of its CapId did not disturb the
shell, and `say`, `ls /`, `fastfetch`, `help` and the prompt kept working — a dead
block driver degrades to a printed error, never a hang, and kernel output still
falls back to PL011 when no console sink exists.

Debug and release QEMU passed Howler-backed TX/RX and sleeping `demo 12`
stop/kill/wait; debug also passed `fg` resume and `uname`. A twelve-second debug
`demo-tasks` run passed 18 markers with exactly two intentional `TASK FAULT`
reports (the same dump is labelled `EL0 SYNC FAULT!` since the exception-path
cleanup):

```text
CONDVAR TEST: beta silent = PASS
LIFECYCLE EXIT/UNREGISTER=PASS
LIFECYCLE MUTEX CLEANUP=PASS
COPYOUT READONLY REJECT=PASS
COPYOUT UNMAPPED REJECT=PASS
HEAP GROW/SHRINK=PASS
MALLOC FREE/REUSE=PASS
MALLOC BRK TRIM=PASS
GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS
GLOBAL ALLOC TRIM=PASS
IPC BP SENDER RESUMED=PASS
IPC TIMEOUT=PASS
IPC BP BACKPRESSURE=PASS
IPC EXIT WAKE=PASS
SECURITY IPC SENDER=PASS
SECURITY MUTEX NONOWNER=PASS
SECURITY MUTEX OWNER=PASS
SECURITY SLEEP OVERFLOW=PASS
```

Debug and release QEMU checks observed `/bin`, `/etc` and `/tmp` in `ls`,
`echo 'hello' > test.txt`, `cat test.txt` printing `hello`, quoted multiword
echo, and `mkdir docs` appearing in the next listing. `kill 3` terminated a
sleeping demo and `wait 3` returned status 1; a naturally exiting echo returned
status 0 through `wait`. The cwd scenario verified `cd /bin` → `pwd` = `/bin`,
empty `/bin` in the inherited `ls`, relative file creation/cat, `cd ..`, nested
`/tmp/docs`, and `cd ../..` back to `/`. Removal tests passed in both profiles:
files and empty directories disappeared, non-empty/root/wrong-type removal was
rejected, and rmdir refused both the current directory and an ancestor of a
nested cwd before succeeding after chdir. Rename tests passed in both profiles
with absolute and relative paths: file contents survived same/cross-directory
moves, old names disappeared, existing destinations remained unchanged, and
same-path/missing-parent/self-subtree/active-cwd moves were rejected. Append
tests passed in both profiles with spaced/unspaced `>>`, existing and new files,
quoted literal operators and a `>` truncation regression check. ANSI prompt and
clear sequences were observed, and uname printed the exact version string. No
`KERNEL PANIC`, `BAD SP` or unexpected `FAIL` occurred. For the DMA/PCI change,
both debug and release `ps` showed a live block_driver task after its DMA
read/write and ECAM-read smoke checks; the 18-marker regression run still
had exactly the two intentional isolation faults.

| Probe | Current ID | Address read from EL0 | Expected FAR |
| --- | ---: | --- | --- |
| High-half kernel | 15 | `0xFFFF000040000000` | `FFFF000040000000` |
| Removed low kernel text alias | 16 | `0x0000000040080000` | `0000000040080000` |

Both have `ESR_EL1 >> 26 = 0x24`, a data abort from a lower exception level.
These are expected translation/permission failures, not kernel regressions.
IDs depend on spawn order and are not a stable userspace API.

Important fixes and invariants from the recent work must be preserved:

1. **The context switch resumes with `ret`, not `eret`.** The saved x30
   returns into `schedule()`, the original call chain unwinds back to the
   exception trampoline, and its epilogue finds the correct 272-byte frame.
2. **UART TX FIFO waits do not freeze the kernel.** `pl011::write_byte()`
   temporarily unmasks IRQs only while waiting for FIFO space and restores
   the caller's DAIF afterwards.
3. **GIC register offsets include the distributor base.** The timer-priority
   write uses `DISTRIBUTOR_BASE + GICD_IPRIORITYR + TIMER_IRQ`.
4. **The syscall message ABI is `#[repr(C)]` on both sides.** Kernel and
   userspace `Message` layouts must match.
5. **SVC decoding preserves live user registers.** `exception_sync_entry`
   saves x9/x10 before reading ESR and restores them before dispatch.
6. **Fault stack dumps use checked copyin.** Each 8-byte word is read through
   `user_memory`; an invalid word prints `<unmapped>` instead of causing a
   secondary EL1 data abort.
7. **Exit must switch even when only idle is ready.** Reaping can reset the
   current index to zero. `next_index == current_index` is skippable only for
   a live task; a dead task still switches to idle and saves into BOOT_CONTEXT.
8. **Scout RPC uses selective receive.** `receive_from(SCOUT_ID)` leaves
   unrelated messages queued. Plain receive could consume a PING as a lookup
   reply, lose the real reply and create a persistent WHAT? loop.
9. **Mutex ownership and wait membership belong to the task.** Reaping removes
   dead waiters, releases owned mutexes and wakes the next FIFO waiter before
   freeing stacks or address spaces.
10. **`brk` growth is all-or-nothing at the API boundary.** On frame exhaustion,
    roll back every page added by the failed request and preserve the old break.
    New mappings are zeroed before becoming visible; unmap and flush before
    freeing a shrunken page.
11. **The userspace free list stays address-sorted.** Coalesce both neighbors
    before trimming. Only the final free block may shrink `brk`, and the shrink
    must succeed before unlinking it. Block sizes include the 16-byte header.
12. **A timed waiter always leaves its condition queue.** Notification pops the
    ID immediately; timeout removal happens in `block_current_on_until` after
    the task resumes. IPC checks for a matching message before checking the
    deadline, so a notify/timeout race cannot discard an available message.
13. **Stopped tasks are woken only through `resume_state`.** `wake_blocked_task`
    and timer expiry update the saved state while leaving `Task.state = Stopped`.
    Only `resume_task` restores it. Never set a stopped task directly to Ready
    from a mutex/condvar/process notification.
14. **IPC queues must outlive their registered waiters.** `remove_task` notifies
    both `not_empty` and `not_full` before dropping a mailbox. This is mandatory
    for kill, including a receiver stopped while blocked on `not_empty`.

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

For bare-metal profiles, `build.rs` invokes Cargo in `userspace/`, uses
`target/userspace` as its target directory, matches debug/release profile and
copies every userspace ELF into `OUT_DIR`. `src/user_program.rs` assigns IDs
0 (demo), 1 (silverd), 2 (Scout), 3 (Howl), 4 (Howler), 5 (ls), 6 (cat),
7 (echo), 8 (say), 9 (sl), 10 (block_driver), 11 (sfs_server),
12 (net_driver), 13 (net_server), 14 (gpu_driver) and 15 (input_driver).
`PROGRAM_DYNAMIC = u64::MAX - 1` marks dynamically loaded (`run`) tasks and is
not part of the table; `hello.elf` is built but only seeded into the VFS. Host
builds skip userspace. Normal boot starts
only silverd; `demo-tasks` starts Scout and the regression task set instead.

Interactive QEMU: `qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M
-nographic -kernel target/aarch64-unknown-none/debug/silver_microkernel`.
Use Ctrl-A then X to quit. For automation, prefix `timeout 5s`; exit code 124
means the kernel lived the whole time. Scripted serial input uses LF (`help\n`);
QEMU's multiplexed stdio may consume a piped bare CR even though the shell
handles both CR and LF from UART.

## Hardware contract (QEMU virt)

- RAM: physical `0x40000000`, 128 MB (defined in `memory.rs` and `-m 128M`)
- UART PL011: physical `0x09000000`; GICv2 Distributor `0x08000000`,
  CPU Interface `0x08010000`. Drivers access them through the high alias.
  The virt DTB describes UART as SPI 1 / level-high: GIC INTID 33, ICFGR `0b00`.
  RXIM + RTIM are unmasked; the RX threshold is 1/8 and the IRQ drains the FIFO.
- Timer: ARM Generic Timer, IRQ 30, 100 Hz tick (`timer::TICK_INTERVAL_MS = 10`).
  UART priority is 0x80 and timer priority 0xA0; ordinary RX does not schedule.
- PCI ECAM: physical `0x4010000000`, 256 MB, kernel-only Device-nGnRnE mapping.
- VirtIO-Net: QEMU `virt`'s implicit default NIC is a transitional `1af4:1000`
  (`1af4:1041` modern-only is accepted too). The driver assigns 64 KiB at
  physical `0x1020_0000`, uses modern common/notify capabilities, queues 0 (RX)
  and 1 (TX), 16 x 2 KiB buffers per queue, and the 12-byte modern virtio-net
  header. No virtio-net IRQ is wired; RX is polled every loop.
- fw_cfg: MMIO at physical `0x09020000` (`data` `+0x00`, `ctl` `+0x08`, `dma`
  `+0x10`). The `dma` register is big-endian and accepts only 4- or 8-byte
  stores; byte stores raise an external abort. `ramfb` (`-device ramfb`) is a
  plain RAM framebuffer configured through the `etc/ramfb` fw_cfg file with a
  big-endian `RAMFBCfg` (fourcc `0x34325241` = ARGB8888).
- VirtIO-Input: `-device virtio-tablet-pci` is a modern-only `1af4:1052` device
  with one event queue (queue 0, 64 entries, 8-byte little-endian events).
  Absolute X/Y are reported `0..32767`. The driver assigns 64 KiB at physical
  `0x1030_0000`.
- FP/SIMD enabled in `_start` (CPACR_EL1 = 0x300000, CPTR_EL2 = 0) —
  without this the first use of a float register traps with EC=7
- Runtime is EL1h for the kernel and EL0t for user tasks: QEMU `-kernel`
  starts at EL2, `_start` drops to EL1 via ERET (HCR_EL2.RW=1,
  SPSR_EL2=0x3C5). VBAR_EL1, both TTBRs, TCR_EL1, MAIR_EL1 and SCTLR_EL1
  are programmed before normal Rust subsystem initialization.
- Translation uses 48-bit VAs on both halves (T0SZ=T1SZ=16), 4 KB granules,
  40-bit PAs and four table levels. MMIO is device-nGnRnE; ordinary DRAM is
  normal WB inner-shareable; user DMA is Normal Non-Cacheable. MAIR entries
  are `0xFF`, `0x00`, `0x44` at indices 0, 1, 2. Data/instruction caches remain
  off (SCTLR.C/I = 0).

## Boot and high-half memory model

`linker.ld` links the kernel at VMA `0xFFFF000040080000`, with physical LMAs
computed as `ADDR(section) - 0xFFFF000000000000`. The ELF entry is the
physical `__boot_entry = _start - __kernel_virt_base` (`0x40080000`), so QEMU
can load and enter the image with the MMU off. Keep VMA/LMA pairs correct for
`.text`, `.rodata`, and `.data`; `.bss` and `.stack` are NOLOAD.

The early assembly stub uses only physical aliases: it sets the boot stack,
drops EL2→EL1, zeroes `.bss`, constructs boot tables, programs both TTBRs,
MAIR/TCR and SCTLR.M, then branches to the linked high PC. Only after that
branch does it install the high SP and VBAR and call `kernel_main`.
`mmu::init()` removes the temporary low identity alias from kernel TTBR0.

Six statically allocated boot tables have fixed 4 KB slots. The assembly fills
the first five; the PCI table is zeroed with `.bss` and populated by `mmu::init()`:

| Index | Table | Role |
| --- | --- | --- |
| 0 | low L0 | Temporary boot TTBR0; emptied after the high-PC transition |
| 1 | high L0 | TTBR1 root |
| 2 | shared L1 | MMIO gigabyte at index 0, DRAM gigabyte at index 1, PCI at index 64 |
| 3 | MMIO L2 | 512 × 2 MB EL1-only device blocks |
| 4 | DRAM L2 | 512 × 2 MB EL1-only normal-memory blocks |
| 5 | PCI L2 | 128 × 2 MB EL1-only device blocks starting at L2 index 128 |

For privileged code, physical address `p` is `0xFFFF000000000000 + p` via
TTBR1. `memory::current()` converts high linker symbols back to PAs so the
frame allocator remains physical-address based. Kernel heap and kernel-stack
virtual addresses, including SP_EL1, are high; allocator logs may separately
show the physical heap base.

Each task owns nine pages of TTBR0 tables plus its image and data pages:

- L0[0] → user L1; L1[1] → user L2; L2[0] → a task-owned image L3.
  The image L3 maps the freshly loaded ELF pages at `0x40000000`.
- L2[1] → a task-owned heap L3 covering `0x40200000..0x40400000`. The table
  starts empty and `SYS_BRK` adds/removes up to 512 data pages.
- L2[511] → a task-owned L3 mapping two stack pages at fixed VA
  `0x7FE00000` (8 KB total). Equal stack VAs have different physical backing.
- L0[1] → a private L1/L2/L3 chain mapping one writable 4 KB page at
  `0x0000008000000000`.
- L1[2] → an on-demand L2/L3 chain for authorized MMIO and DMA mappings at
  `0x80000000`. These two table pages are allocated only on first use.
- There are no user mappings for kernel text/rodata, kernel `.data` or `.bss`,
  kernel heap, kernel stacks, unrelated DRAM, page-table pages or unauthorized
  MMIO.
- `destroy_task_space()` frees all task-owned tables, heap frames and the
  private page; `Task::release_space()` then releases ELF and tracked DMA frames.

## ELF loading and W^X

`userspace/linker.ld` links a static `ET_EXEC` image at `0x40000000` with
separate R-X text, R-- rodata and RW data `PT_LOAD` segments. `.bss` is
`NOLOAD` and belongs to the RW segment, so its memory size can exceed its
file size.

`elf::load_elf()` accepts ELF64 little-endian AArch64 `ET_EXEC` files only. It
checks the ELF header, program-header and file bounds, requires every loadable
segment to fit within the 2 MB image window, rejects W+X segments and rejects
different permissions sharing one 4 KB page. The entry must lie in an
executable segment. It allocates fresh contiguous frames, zeroes the whole
image, copies `PT_LOAD` file bytes and records a `PageKind` for every page,
returning an owned `LoadedImage { physical_start, page_count, entry, kinds }`.
`elf_loader::load()` is a thin wrapper over it that logs the rejection reason;
both the embedded `spawn` path and the dynamic `run <path>` path share this one
loader.

The resulting mappings enforce W^X: text is R-X at EL0 and PXN; rodata is R--
and UXN/PXN; data, heap, stack and private memory are RW at EL0 and UXN/PXN.
Each spawn loads another copy, so even writable ELF data/BSS is private. VM A/B
write `0xAAAA` / `0xBBBB` to their private page, `ELF_BSS_CELL` and heap, then
read back only their own values.

## EL0 execution and lifecycle model

- `SYS_SPAWN(program, argument)` selects any embedded program ID from the table
  above. `scheduler::spawn_user` creates a kernel stack, an 8 KB user stack, a
  private page and an independent ELF copy before constructing TTBR0. Only idle
  remains an EL1 task.
- A fresh user context has high SP_EL1, the ELF entry in x19, the argument in
  x20 and SP_EL0 = `0x7FE00000 + 0x2000`. `silver_user_bootstrap` moves x20 to
  x0, sets ELR_EL1 = x19 and SPSR_EL1 = 0, then executes `eret`.
- Demo roles are selected by the argument in `userspace/src/main.rs`, not by
  task ID. Silverd, Scout, Howl, Howler and utilities have separate executables
  in `userspace/src/bin/`.
- The scheduler switches only TTBR0_EL1, then performs
  `tlbi vmalle1; dsb ish; isb`. High kernel text and SP_EL1 remain valid
  through TTBR1 throughout the switch.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`, which prints
  `Task <id> killed: Page Fault at 0x... (ESR: 0x...)` plus `ELR_EL1`, dumps
  the switch trace and kills only that task; EL1 goes to
  `silver_handle_exception`, which dumps kind/ESR/ELR/FAR and halts (kernel
  panic).
- Syscalls include sleep, IPC, nameserver, print, task control, spawn/args,
  dynamic ELF spawn (`SYS_SPAWN_PATH`), wait/kill, stop/resume, `brk`, UART
  read, console ownership and RAM-VFS.
- `SYS_CONSOLE_SET(cap_id)` requires `CAP_CONSOLE_GRANT`, then rejects idle,
  missing and dead target IDs. On success it makes the selected task the sole
  interactive UART owner under one IRQ-critical
  section and records the calling shell as `console_parent` for Ctrl-Z. Print
  syscalls from non-owners succeed but discard their bytes; non-owner `SYS_READ`
  returns SYSCALL_ERROR without touching PL011.
- On the normal path, console print syscalls copy and validate user bytes, then
  send `[1, len, data]` chunks to the console sink with `ipc::try_send`. The sink
  is looked up per call through `scheduler::console_sink_task()` (the live task
  bound to INTID 33), so a shell-spawned task cannot capture the output stream.
  Successful sends set the sink's pending event, wake its `wait_irq` and yield
  the CPU. Howler polls
  output IPC with zero timeout and waits for TXFF before writing DR. A dead
  Howler selects the kernel PL011 fallback; a full mailbox drops remaining bytes.
- The unbound PL011 fallback drains INTID 33 into a 256-byte software FIFO and
  consumes Ctrl-Z as a stop request. The owner's `SYS_READ` reads only that FIFO;
  no byte/no input returns SYSCALL_ERROR. On the normal path Howler reads DR,
  clears RX/RT interrupt sources and either sends `[0, byte]` to Howl or invokes
  `SYS_STOP_CONSOLE` for Ctrl-Z. Timer IRQs handle ticks and normal scheduling.
  `SYS_UPTIME` returns raw 100 Hz ticks. `SYS_MEMINFO` copies three u64 byte counts
  through checked copyout; used includes kernel/reserved RAM and allocated frames.
- `SYS_TASK_LIST(buffer, capacity)` copies up to 64 `#[repr(C)]` TaskInfo entries
  (ID, numeric state, program, argument). State 7 is Stopped. Task metadata lives
  in the Task itself, so ps can distinguish identical ELF roles.
- `SYS_SPAWN_ARGS(program, numeric, ptr, len)` copies at most 192 argument bytes
  before allocating the task. `SYS_ARGS(buffer, capacity)` returns those bytes
  to the current program and fails rather than truncating them.
- `SYS_WAIT(cap_id)` requires `CAP_WAIT`, blocks only for termination and consumes
  one of 64 bounded completion records. `SYS_WAIT_EVENT(cap_id)` instead returns
  Stopped as `SYSCALL_STOPPED = u64::MAX - 2` when the target is stopped.
  `SYS_TASK_RESUME(cap_id)` requires `CAP_RESUME`, restores the saved state and is
  a no-op for a live non-stopped task. `SYS_KILL(cap_id)` requires `CAP_KILL`,
  rejects idle/self and records status 1; SYS_EXIT records its status.
- `SYS_SEND(target, message)` blocks while the target has 16 queued messages and
  returns `SYSCALL_ERROR` if the target is dead. `SYS_TRY_SEND` has the same
  checked-copyin and sender-stamping path but returns immediately on a full or
  dead mailbox; the UART driver drops that byte and continues its IRQ loop.
  After copyin the kernel always replaces `message.sender` with
  `current_task_id()`, making selective receive and nameserver replies resistant
  to sender spoofing. Nameserver RPC paths propagate send failure and parse
  returned IDs with checked arithmetic.
- `SYS_SLEEP(ms)` uses a saturating absolute deadline, so an extreme userspace
  argument cannot panic a debug kernel or wrap to an immediate wakeup.
- `SYS_MUTEX_UNLOCK` succeeds only for the recorded owner in every build profile;
  non-owner attempts return `SYSCALL_ERROR` without waking a waiter.
- `SYS_RECEIVE(buffer, timeout_ticks)` uses zero for a poll and `u64::MAX` for an
  infinite wait. Other values are relative 100 Hz ticks; expiry returns
  `SYSCALL_TIMEOUT = u64::MAX - 1`. Invalid buffers still return SYSCALL_ERROR.
- `SYS_BRK(0)` queries the program break. Other values set an absolute break in
  `0x40200000..0x40400000`; invalid requests and allocation failures return
  `SYSCALL_ERROR` without changing the previous break or mappings.
- `SYS_MAP_PHYS(phys, size)` rejects empty, overflowing and DRAM ranges. Its
  lazily allocated L2/L3 tables are freed with the task address space, while the
  referenced device frames are never freed. Each mapping records a `CAP_MMIO`
  region token and returns the user VA including the original page offset.
- A bound IRQ bypasses the kernel device handler: the GIC distributor masks it
  before EOI, then a pending flag and wakeup go to its live owner. `bind_irq`
  and `SYS_WAIT_IRQ` unmask it only when EL0 is ready for another event.
  `SYS_WAIT_IRQ` preserves events delivered before the wait. Exit, kill and
  reaping clear the binding, restoring the kernel fallback; timer and spurious
  INTIDs cannot be bound.
- Exit and kill clear the task mailbox, wake both receiver and sender waiters
  before freeing its condvars, and close every VFS descriptor owned by that task.
  If the dead task owned the interactive console, `recover_console` gives it to
  the first live Howl before scheduling; `demo-tasks` instead only clears
  the dead task's shared-console flag. Reaping removes stale waiters, releases
  owned mutexes, wakes the next waiter, then frees ELF pages, heap frames, page
  tables, private page and both stacks. A woken sender rechecks liveness and
  cannot recreate the dead task's mailbox.
- Nameserver unregister is owner-checked. Its table is also purged through
  `task_alive` when later requests arrive, so names left by dead tasks do not
  remain permanently valid.
- User tasks cannot use the high-only kernel heap. The nameserver therefore
  stores 16 fixed-capacity entries on its user stack instead of in a `Vec`.

## RAM-VFS

- `Node` is either `File(Vec<u8>)` or `Directory(Vec<Node>)`. The volatile root
  starts with `/bin`, `/etc` and `/tmp`. Each Task stores an absolute cwd, and
  `spawn_inner` clones it into the child. Relative paths resolve from that cwd;
  `.` and `..` are normalized and `..` cannot escape root.
- Open handles store an absolute normalized path rather than a Node pointer or
  relative path, so Vec reallocation and later chdir calls cannot retarget them.
  Each handle has owner, per-owner fd (starting at 3), offset, writable and
  append flags. Exit/kill removes all handles for that owner.
- `SYS_FS_OPEN/READ/WRITE/CREATE/CLOSE/READ_DIR/MKDIR/UNLINK/RMDIR/RENAME` use
  checked copyin/copyout. `SYS_CHDIR` resolves and validates a directory before
  replacing the Task cwd; `SYS_GETCWD` copies its absolute bytes. Paths are
  limited to 191 bytes, names to 32 bytes and one I/O call to 4096 bytes.
  `create` truncates files, `open` supports read-only, write and write+append
  modes. Append starts at the current EOF on every write, not merely at open.
  `read_dir` copies 48-byte `#[repr(C)]` entries with name bytes/length and kind.
- `unlink` removes only files with no open handles; there is deliberately no
  Unix-style unlink-while-open while handles are path-based. `rmdir` removes
  only empty, non-root directories and rejects a directory used at or above any
  live task's cwd. The cwd check runs inside the same single-CPU IRQ-critical
  section as removal, so chdir/spawn cannot interleave with it.
- `rename` validates both paths and the destination parent before detaching the
  source. It never overwrites, rejects a no-op path and prevents directory
  cycles. Sources with open files below them or live cwds below directories are
  rejected instead of rewriting path-based handles/cwds. The same IRQ-critical
  section covers checks and the move, so expected failures preserve the tree.
- There is no persistence or file-size quota yet. Repeated writes extend the
  in-memory file through the kernel heap.

## Howl (silvershell.rs)

- Howl is an independent no_std/no_main ELF with no allocator. It uses a
  96-byte line buffer, accepts printable ASCII and CR/LF, echoes input, and
  implements Backspace/DEL by removing the last byte and erasing it onscreen.
- Normal boot is automatic: silverd starts Scout, waits (bounded) for its
  `SCOUT READY` IPC, starts block_driver with PCI/MMIO rights, then starts Howl
  and hands it the console, and finally starts Howler with Howl's ID as its input
  target. There is no manual `start_uart_driver` command. Howler maps PL011,
  binds INTID 33 and sends `[0, byte]` input messages directly to Howl's mailbox.
  Howl polls with a zero timeout, accepts only type-0 messages from the Howler it
  resolved from the task table, and sleeps 1 ms when empty. That ID is re-resolved
  from `SYS_TASK_LIST` at most once per second, so a respawned Howler is adopted
  automatically and a dead one is detected — `silverd` never has to know Howler's
  number in advance. Normal output uses SYS_PRINT/SYS_PRINT_HEX/SYS_PRINT_DEC,
  whose typed chunks return through IPC to Howler for the actual PL011 writes.
- Startup emits ANSI clear/home, runs fastfetch once and shows a bold cyan
  `howl>` prompt. Commands are `help`, `fastfetch`, `ls`, `cat`, `echo`, `say`,
  `sl`, `run`, `mkdir`, `rm`, `rmdir`, `mv`, `cd`, `pwd`, `clear`, `uname`, `ps`,
  `spawn`, `bg`, `fg`, `kill` and `wait`; empty lines only reprompt. Echo parsing
  preserves quoted spaces, strips outer quotes and recognizes `>`/`>>` only
  outside quotes. `mv old new` takes a full destination path, not just a directory.
- `ls`, `cat`, ordinary `echo` and `say` spawn separate EL0 utilities with
  string args and inherited cwd. `say` preserves spaces and strips paired outer
  quotes. `sl` runs a 60-frame ANSI steam-locomotive animation. `run_utility`
  gives the child the console and uses wait_event for Exited/Stopped events.
  `echo text > path` truncates/creates; `echo text >> path` appends/creates
  directly through VFS. `mkdir`, `rm`, `rmdir`, `mv`, `cd` and `pwd` are direct
  syscalls.
- `ps` uses a fixed 32-entry userspace array and prints ID/program/argument/state,
  including `stopped`. `spawn` and `bg` parse an optional checked decimal u64 and
  create a task without console ownership. `fg <cap>` calls resume, transfers the
  console and waits for an Exited/Stopped event. Howler consumes Ctrl-Z and calls
  `SYS_STOP_CONSOLE`, so the foreground task stops without receiving byte 0x1A.
  `kill` works on stopped tasks, and plain `wait` still reports only exit status.
- Background task output is dropped rather than buffered, so early output cannot
  be replayed by `fg`. Only output produced after the transfer becomes visible.
- Fastfetch converts ticks to seconds with hundredths and bytes to tenths of a
  MiB. Its three memory counters come from SYS_MEMINFO, not hardcoded free RAM.
  The architecture row is `Kernel: AArch64`.
- Normal boot starts only silverd (task 1) in the kernel; silverd creates Scout,
  block_driver, net_driver, sfs_server, net_server, Howl and Howler in that
  order. Nothing in userspace or the kernel reads those numbers: the console
  sink, Howler, block_driver, net_driver and net_server are all resolved from
  live scheduler state. `demo-tasks` does not start silverd: Scout remains task 1,
  the earlier regression IDs are preserved and every demo opts into shared UART
  output.

## Userspace allocation

- `userspace/src/allocator.rs` stores a 16-byte header before every payload;
  header `size` is the total block size and `next` is used only while free.
- `malloc` aligns payloads to 16 bytes, returns null for zero/overflow/OOM,
  uses first-fit and splits when at least 32 bytes remain. If no block fits,
  it extends `brk` and creates a new block there.
- `free` inserts by address and coalesces with both neighbors. If the final
  free block ends at the current break, `trim_program_break` first shrinks
  `brk`, then unlinks that block. Thus tail memory returns to the kernel.
- Allocator state lives in each task's private ELF data. It currently assumes
  one execution thread per address space and has no double-free validation.
- The demo ELF registers `UserAllocator` as `#[global_allocator]` and imports
  `extern crate alloc`. `alloc` forwards `alloc`/`dealloc` to `malloc/free`;
  default `realloc` uses those operations plus a copy.
- Layout alignment above 16 bytes is not supported yet. It and OOM print the
  layout and exit the current EL0 task. This explicit error path avoids the
  still-unstable `#[alloc_error_handler]`; do not return null into `alloc`.

## Page-table, copyin/copyout and PAN rules

- User ELF text is AP=0b11 (read-only at EL1 and EL0), executable at EL0 and
  PXN. User rodata is read-only and UXN/PXN.
- User data, heap pages, stacks and private pages are AP=0b01 and UXN/PXN.
  TTBR1 MMIO and DRAM mappings are AP=0b00 and absent from TTBR0.
- AP=0b10 is not user read-only: with the two-range EL1/EL0 regime it means
  read-only at EL1 and no EL0 access. Use AP=0b11 for user-readable rodata.
- A page writable at EL0 (AP=0b01) is execute-never at EL1 regardless of PXN.
  Kernel execution remains in TTBR1; user text is read-only and PXN.
- `mmu::valid_user_slice()` requires EL0-read permission; the `_mut` variant
  requires AP=0b01 write permission. Every page in the range is walked through
  current TTBR0 and the 4096-byte cap remains.
- `user_memory::copy_from_user` uses `LDTRB`; `copy_to_user` uses `STTRB`.
  Kernel-side sources and destinations are ordinary high-half buffers. No
  arbitrary kernel code should dereference a raw user pointer directly.
- PAN is enabled in both the boot stub and `configure_and_enable` (SCTLR bit
  22). LDTR/STTR perform unprivileged access even with PAN set.
- `sys_receive` validates the output buffer for write access before blocking,
  then copyout checks it again after wakeup. Read-only and unmapped buffers
  return SYSCALL_ERROR without consuming a message.

## Architectural invariants (do not break!)

1. **Single-CPU.** Every `unsafe impl Sync` on an UnsafeCell wrapper
   (`TaskManager`, `IpcState`) rests on a single CPU core. Real spinlocks
   come before SMP.
2. **Critical sections via DAIF.** Global kernel state (scheduler, IPC) is
   mutated only under `save_and_disable_irq` / `restore_irq`. That is the
   only mutual-exclusion mechanism besides Mutex + Condvar.
3. **The DAIF rule for tasks.** A voluntary context switch (`sleep` /
   `yield` / `block_current_on`) inherits masked IRQs. Kernel task entry points
   must call `interrupts::enable_irq()` before waiting; idle does this.
   User tasks get unmasked PSTATE from `silver_user_bootstrap` and from the
   SPSR saved in their exception frames.
4. **Timer and UART IRQs have separate paths** (`gic::silver_handle_irq`).
   Timer: `timer::handle_irq()` → EOI → `schedule()`. A bound non-timer IRQ takes
   distributor mask → EOI → set pending/wake its user owner without entering
   the kernel driver; `bind_irq`/`SYS_WAIT_IRQ` unmask it for the next event.
   Unbound UART: `pl011::handle_irq()` drains RX → consume stop request →
   optional `stop_console_owner()` → EOI → `schedule()` only if a task stopped.
   Ordinary unbound UART bytes never cause a context switch. PL011 is
   level-sensitive: its ICFGR field must remain `0b00`.
5. **The heap is initialized after the frame allocator** and only through
   `alloc_contiguous_pages`. No `alloc` before `allocator::init()`.
6. **Context switch layout:** x19-x30 at offsets 0-95, SP_EL1 at 96,
   SP_EL0 at 104, resume PC (`TaskContext.elr`) at 112 — 120 bytes total.
   `silver_context_switch` saves/restores this block and finishes with
   `ret`. On save, x30 is copied into the resume-PC slot; on restore, the
   resume-PC slot wins over the saved x30. Fresh tasks put their entry or
   `silver_user_bootstrap` there.
7. **Task states:** `Ready / Running / Sleeping(deadline_ticks) /
   Blocked / BlockedUntil(deadline_ticks) / Stopped / Dead`. The scheduler gives
   the CPU only to Ready tasks. Stopped tasks keep a separate resume_state;
   timer expiry and wakeups update that saved state without scheduling them.
   `find_next_task` prefers non-idle tasks.
8. **Both trampolines keep ELR/SPSR on the stack.** Each builds a 272-byte
   frame: x0-x30, ELR at offset 248, SPSR at offset 256. The epilogue
   restores ELR/SPSR from the frame before `eret`. They mask IRQs on entry
   and do not need to re-arm ELR/SPSR for scheduling: the ret-based switch
   unwinds back to the epilogue with the original SP. CPU-global ELR/SPSR
   may be clobbered by other tasks in between; the frame copy is authoritative.
9. **SVC register preservation starts before ESR decoding.** x9/x10 are user
   registers, not free scratch. The sync entry saves/restores them around its
   dispatch check; the full 272-byte frame is built afterwards.
10. **Scout holds the well-known SCOUT_ID**, fixed by spawn order: 2 in normal
    boot and 1 in `demo-tasks`. Protocol: text over mailbox messages,
    "R <name>" → "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING", and
    "U <name>" → "OK"/"MISSING"/"NOT OWNER". Kernel RPC uses
    `receive_from(SCOUT_ID)`, so unrelated queued messages are preserved.
11. **Cross-crate syscall structures have a stable layout.** `Message` is
    `#[repr(C)]` in kernel and userspace. Apply the same rule to future
    structs passed by pointer across the SVC boundary.
12. **TLB/cache ritual for any future table edit.** After modifying tables,
    clean lines to the point of coherency (`dc cvac` + `dsb`), invalidate
    stale TLB entries (`tlbi` + `dsb` + `isb`). Template:
    `mmu::flush_after_table_change`. Address-space switches load TTBR0_EL1
    and issue `tlbi vmalle1; dsb ish; isb` before the context switch.
13. **UART FIFO waits preserve DAIF.** `pl011::write_byte()` may unmask IRQs
    while waiting for TXFF, but restores the caller's saved DAIF before
    returning. Never replace this with an indefinitely masked spin. Taking
    a timer IRQ mid-print is intentional and safe with the frame-based
    trampolines.
14. **Fault diagnostics must not create a second fault.** Validate every
    userspace stack word before printing it; print `<unmapped>` at the first
    invalid word. (The SP_EL0 stack walk itself is not emitted right now — see
    Debugging aids — but the rule stands as soon as it comes back.)
15. **The network mailbox length word is the publication barrier.** Write the
    payload first, `dmb ish`, then the length; on consume, read the length,
    `dmb ish`, copy, then clear. The window is Normal Non-Cacheable; volatile
    accesses alone do not order the payload against the length.
16. **One frame slot per direction, with backpressure.** Publish only when the
    length word is zero. Never write `RX_FRAME`/`TX_FRAME` while the peer owns
    it. `MAX_FRAME` is 1536 bytes; every copy is clamped to it.
17. **Bind the mailbox to one server task.** `OP_TX` and RX doorbells use the
    `server` task ID recorded by `OP_ATTACH`. A re-attach may refresh that ID
    but must not map the same physical window twice.
18. **Use the NIC MAC from `OP_ATTACHED`, never the QEMU default.** The network
    server replaces its initial hardware address before sending; otherwise ARP
    advertises the wrong source. Resolve both network tasks by program ID, not
    by fixed task numbers.

## Why IPC uses Mutex + Condvar

IPC state lives behind `Mutex<IpcState>`. Every mailbox owns boxed `not_empty`
and `not_full` Condvars; boxing keeps their addresses stable when the mailbox
Vec reallocates. `Condvar::wait`/`wait_until` releases the mutex before blocking.
Send wakes one receiver, receive wakes one sender, and mailbox destruction wakes
all senders. Check the condition and wait under the lock, notify under the lock,
and re-check after waking. Timed waits share the scheduler's absolute tick
deadlines and remove themselves from the queue on every return path.

IRQ masking guards the tiniest scheduler-critical sections; Mutex + Condvar
guard everything that can wait on a condition. With a bounded queue, a task that
selectively waits for one sender can leave its mailbox full and block the very
sender it needs. Likewise, nameserver replies obey backpressure and can block if
a client's mailbox is full; demos must not issue RPC to an already-full mailbox.

## Debugging aids

`scheduler.rs` keeps a 16-entry ring of recent context switches
(`from`, `to`, next SP, next resume PC). `silver_handle_task_fault` prints the
fault registers and then dumps that ring before killing the task:

```text
Task <id> killed: Page Fault at 0x<far> (ESR: 0x<esr>)
ELR_EL1=<elr>
SWITCH TRACE from,to,sp,elr:
<from>-><to> sp=<next sp> elr=<resume pc>
```

It costs no output during normal operation; keep it until task lifecycle and
isolation are more battle-tested. The handler only pokes PL011 registers: it
never allocates and never takes a lock, because the dying task may still own
either. Not emitted today, although earlier revisions had them: the `EC` field
(`esr >> 26`) and the eight-word SP_EL0 stack walk that stops at the first
unmapped word.

QEMU exception tracing is invaluable for resume/fault bugs:

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel \
  -d int -D qemu-int.log
```

## Known tech debt

- The loader supports only static `ET_EXEC`, a contiguous 2 MB image window
  and page-uniform permissions; there is no PIE, dynamic linking, demand
  loading or sparse multi-window image layout. `run <path>` loads a whole file
  into kernel memory before parsing it (bounded by `EXEC_FILE_LIMIT`), and the
  dynamic task's argument is always `0`.
- Programs come either from ELFs embedded at kernel build time or from the
  RAM-VFS via `run <path>`; there is no persistent on-disk program storage (the
  on-disk SFS server still exposes only the superblock) and no program
  credential model. Task control and rights delegation use per-task
  capabilities, but any task can still request an embedded shell ELF or load
  any VFS file it can name.
- Mutex and condvar wait queues still have no growth limits. Mailboxes are
  bounded at 16 messages, but nameserver/control replies also obey backpressure;
  a full client mailbox can therefore block the nameserver. Selective receive
  on a full mailbox can likewise deadlock with its expected sender. Spawn counts,
  kernel heap/VFS growth and per-task memory consumption need quotas for DoS
  resistance.
- The user heap is limited to one 2 MB L3 window. The allocator supports only
  alignments up to 16 bytes and has no explicit C-style `realloc`/`calloc`,
  thread safety or double-free detection. There is no general `mmap`/`munmap`
  or growable stack; the user stack remains fixed at 8 KB and the private
  window at one page.
- Interactive mode prevents ordinary background/foreground output interleaving,
  but `demo-tasks` intentionally shares UART. Kernel diagnostics bypass ownership,
  and an IRQ may schedule another task during a FIFO wait. UART RX is interrupt-
  driven but has no escape-sequence parser and drops new bytes when its 256-byte
  buffer is full. Background output is discarded. There are no process groups,
  `jobs` command or general signal API.
- `#[alloc_error_handler]` is unstable on stable Rust. The kernel converts a
  null from its `GlobalAlloc` into a panic; the demo userspace wrapper instead
  prints the layout and exits only the current EL0 task.
- The block driver's `INIT_NOT_FOUND` stage (1) is overloaded: `find_virtio_blk()`
  returns `None` both when the ECAM scan matches no device and when any `?`
  fails *after* a match (capability parsing, `assign_bar4`, `map_phys`). The
  2026-10-27 BAR0 incident showed how expensive that ambiguity is — stage 1
  pointed at "no device on the bus" while the device had been found and
  dropped. Split the stage into "scan found nothing" and "device found, setup
  failed" (the client report then names the exact step).
- Networking is IPv4-only with a static `10.0.2.15/24` address and gateway
  `10.0.2.2`. `net_server` polls every 10 ms; one frame fits per direction;
  there are no virtio-net interrupts, TCP/UDP services or DHCP/DNS. The shell
  `ping` is single-flight ICMP only (no `net`/TCP command surface yet). The
  driver assigns its 64 KiB BAR at a fixed `0x1020_0000` on the assumption that
  the block driver keeps `0x1010_0000`. There is no detach/unmap protocol, so a
  dead `net_server` leaves the driver holding the old mapping until the next
  attach. TX-ring exhaustion drops the frame (`NET DRIVER: TX FRAME DROPPED`)
  rather than blocking the server.
- The graphics menu is single-buffered: `gpu_driver` repaints the entire
  640x480 frame (clear + menu + cursor) on every pointer sample, with no
  dirty-region tracking; `ramfb` has no hardware flip or vsync. The input driver
  polls the virtio event queue at ~100 Hz instead of using a virtio IRQ, and
  assumes QEMU's fixed `0..32767` absolute range. The input driver's 64 KiB BAR
  is fixed at `0x1030_0000`, extending the fixed-BAR assumption already used by
  the block (`0x1010_0000`) and net (`0x1020_0000`) drivers.
- The EL1/kernel exception path now halts (`silver_handle_exception` dumps the
  registers and spins in a `wfe` loop) instead of resuming the current task;
  see the Phase 7 section. The remaining gap is that the halt is a bare spin —
  it does not reset the machine or turn the fault into a debuggable
  core/register dump for offline triage.


## Next steps (by priority)

1. **Graphics/input polish.** Double-buffer the menu and repaint only the dirty
   region under the cursor/button instead of the whole frame; wire virtio-input
   interrupts to replace the 100 Hz poll; read `ABS_INFO` min/max from the
   device config instead of assuming `0..32767`; and turn the six menu buttons
   into real actions.
2. **Program credentials:** define which embedded programs a task may request;
   currently any task can still spawn the shell ELF. Capability delegation is
   available through `SYS_CAP_GRANT`.
3. **Resource quotas:** bound task count, per-task/kernel memory, wait queues and
   VFS file growth; define behavior under exhaustion before adding more services.
4. **Network services:** add TCP/UDP sockets on smoltcp, DHCP/DNS configuration
   and a `net` command surface beyond `ping`. Replace the 10 ms RX poll with
   virtio-net interrupts, then add multiple frames per direction and zero-copy
   or scatter-gather TX.
5. **SFS write path and persistence:** block-path interrupts, write support,
   file-size quotas and overwrite/rename options; then keep the image across
   reboot.
6. **Generalize per-task VM:** sparse multi-window ELF images, `mmap`/`munmap`,
   growable stacks and demand paging later. Add over-aligned user allocations
   and explicit `realloc`/`calloc` when their use cases appear.
7. **Dynamic loader follow-ups:** pass argv/`char*` to `run`'d programs, stream
   segments instead of slurping the whole file, load from the on-disk SFS once
   it can read files, and add PIE/dynamic linking later. A program-credential
   model should gate both `spawn` and `run` (see step 2).
8. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

Nameserver watch notifications are explicitly deferred. Later IPC control-plane
work should also address nonblocking/priority replies, send timeouts/cancellation
and the full-mailbox selective-receive limitation recorded above.

## Workflow for a new feature

1. Code → `cargo fmt --all` and `cargo fmt --manifest-path userspace/Cargo.toml`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. Also check `--release`: optimized userspace exposes register-preservation
   bugs that debug builds can hide. For regressions, separately build with
   `--features no_std,demo-tasks`; this replaces the interactive boot.
5. For the default profile, verify `ls`, `echo 'hello' > test.txt`,
   `cat test.txt`, `mkdir docs`, quoted echo, `cd /bin`, `pwd`, relative paths,
   `cd ..`, `ps`, and `spawn`/`kill`/`wait`. Also check fastfetch and the logo,
   confirm `ping 8.8.8.8` prints `PING 8.8.8.8: OK`, and make sure `ps`
   lists live `net_driver` and `net_server` tasks.
   Run `bg demo 12`, confirm shell output stays free of `AWAKE`, then `fg 3`.
   Send Ctrl-Z, verify `ps` = stopped and silence while stopped, resume with
   `fg 3`, stop again and verify kill/wait. Repeat with blocked `demo 7` and
   verify a notification received while stopped is delivered after resume.
   For demo-tasks, run 5–30 seconds,
   repeat timing-sensitive runs and expect exactly two intentional `EL0 SYNC
   FAULT!` blocks from tasks 15 and 16, plus
   `COPYOUT READONLY REJECT=PASS`, `COPYOUT UNMAPPED REJECT=PASS`,
   `LIFECYCLE EXIT/UNREGISTER=PASS`, `LIFECYCLE MUTEX CLEANUP=PASS`,
   `HEAP GROW/SHRINK=PASS`, `MALLOC FREE/REUSE=PASS`,
   `MALLOC BRK TRIM=PASS`, `GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS`,
   `GLOBAL ALLOC TRIM=PASS`, `IPC TIMEOUT=PASS`,
   `IPC BP SENDER RESUMED=PASS`, `IPC BP BACKPRESSURE=PASS`,
   `IPC EXIT WAKE=PASS`, `SECURITY IPC SENDER=PASS`,
   `SECURITY MUTEX NONOWNER=PASS`, `SECURITY MUTEX OWNER=PASS` and
   `SECURITY SLEEP OVERFLOW=PASS`.
6. Update the README checklist and this file.
