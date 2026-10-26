# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-26 (DMA/PCI infrastructure and Ring 3 UART)

Latest verified architecture: **silverd -> Scout, block_driver, Howl, Howler**.
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
Howler in Ring 3: input messages are `[0, byte]`, while output messages are
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
reports:

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

Both report `EC=0x24`, a data abort from a lower exception level. These are
expected translation/permission failures, not kernel regressions. IDs depend
on spawn order and are not a stable userspace API.

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
7 (echo), 8 (say), 9 (sl) and 10 (block_driver). Host builds skip userspace. Normal boot starts
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

`elf_loader::load()` currently accepts ELF64 little-endian AArch64 `ET_EXEC`
files only. It checks program-header and file bounds, requires every loadable
segment to fit within the 2 MB image window, rejects W+X segments and rejects
different permissions sharing one 4 KB page. The entry must lie in an
executable segment. It allocates fresh contiguous frames, zeroes the whole
image, copies `PT_LOAD` file bytes and records a `PageKind` for every page.

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
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`; EL1 is a
  kernel panic.
- Syscalls include sleep, IPC, nameserver, print, task control, spawn/args,
  wait/kill, stop/resume, `brk`, UART read, console ownership and RAM-VFS.
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
  `sl`, `mkdir`, `rm`, `rmdir`, `mv`, `cd`, `pwd`, `clear`, `uname`, `ps`,
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
  block_driver, Howl and Howler in that order (currently tasks 2-5). Nothing in
  userspace or the kernel reads those numbers: the console sink, Howler and
  block_driver are all resolved from live scheduler state. `demo-tasks` does not
  start silverd: Scout remains task 1, the earlier regression IDs are preserved
  and every demo opts into shared UART output.

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
    invalid word.

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
(`from`, `to`, next SP, next resume PC). `silver_handle_task_fault` dumps it
before killing the task. It costs no output during normal operation; keep it
until task lifecycle and isolation are more battle-tested. Fault stack dumps
stop at the first unmapped word.

QEMU exception tracing is invaluable for resume/fault bugs:

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel \
  -d int -D qemu-int.log
```

## Known tech debt

- The loader supports only static `ET_EXEC`, a contiguous 2 MB image window
  and page-uniform permissions; there is no PIE, dynamic linking, demand
  loading or sparse multi-window image layout.
- Programs are selected from ELFs embedded at kernel build time; there is no
  external program storage or program credential model. Task control and rights
  delegation use per-task capabilities, but any task can still request an
  embedded shell ELF.
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

## Next steps (by priority)

1. **Program credentials:** define which embedded programs a task may request;
   currently any task can still spawn the shell ELF. Capability delegation is
   available through `SYS_CAP_GRANT`.
2. **Resource quotas:** bound task count, per-task/kernel memory, wait queues and
   VFS file growth; define behavior under exhaustion before adding more services.
3. **Grow VFS semantics:** file-size quotas and overwrite/rename options if
   needed. Persistent storage still needs a block driver.
4. **Generalize per-task VM:** sparse multi-window ELF images, `mmap`/`munmap`,
   growable stacks and demand paging later. Add over-aligned user allocations
   and explicit `realloc`/`calloc` when their use cases appear.
5. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.
6. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
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
   `cd ..`, `ps`, and `spawn`/`kill`/`wait`. Also check fastfetch and the logo.
   Run `bg demo 12`, confirm shell output stays free of `AWAKE`, then `fg 3`.
   Send Ctrl-Z, verify `ps` = stopped and silence while stopped, resume with
   `fg 3`, stop again and verify kill/wait. Repeat with blocked `demo 7` and
   verify a notification received while stopped is delivered after resume.
   For demo-tasks, run 5–30 seconds,
   repeat timing-sensitive runs and expect exactly two intentional faults from
   tasks 15 and 16, plus
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
