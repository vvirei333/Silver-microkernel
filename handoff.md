# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24 (ELF userspace update)

Latest verified feature: **a separately linked static AArch64 userspace ELF is
loaded into an independent set of pages for every EL0 task**. The old shared
kernel `.text` / `.rodata` alias is gone. User TTBR0 now contains only the
current task's own ELF image, user stack and private page. Kernel image/data,
heap, kernel stacks, page tables and MMIO exist only in the TTBR1 high half.

Verified with host, debug bare-metal and release bare-metal builds, a release
QEMU smoke run and five consecutive 8-second debug QEMU runs. Every run had
exactly two intentional `TASK FAULT` reports and no `KERNEL PANIC`, `BAD SP`,
`ISOLATION FAIL`, `ALIAS FAIL` or `VM ISOLATION FAIL`. IPC, nameserver,
mutex/channel, condvar, sleep and both VM isolation checks continued after the
probe tasks died.

| Probe | Current ID | Address read from EL0 | Expected FAR |
| --- | ---: | --- | --- |
| High-half kernel | 15 | `0xFFFF000040000000` | `FFFF000040000000` |
| Removed low kernel text alias | 16 | `0x0000000040080000` | `0000000040080000` |

Both report `EC=0x24`, a data abort from a lower exception level. These are
expected translation/permission failures, not kernel regressions. IDs depend
on spawn order and are part of the current demo contract, not a stable API.

Important fixes from the EL0/high-half and ELF work must be preserved:

1. **The context switch resumes with `ret`, not `eret`.** The saved x30
   returns into `schedule()`, the original call chain unwinds back to the
   exception trampoline, and its epilogue finds the correct 272-byte frame.
   The old eret-based switch could read garbage as ELR and resume at 0x2.
2. **UART TX FIFO waits do not freeze the kernel.** `pl011::write_byte()`
   temporarily unmasks IRQs only while waiting for FIFO space and restores
   the caller's DAIF afterwards. An indefinitely masked TXFF spin prevented
   timer preemption.
3. **GIC register offsets include the distributor base.** High-half MMIO
   exposed a latent timer-priority write to bare offset `0x41E`, causing
   `KERNEL PANIC EC=0x25 FAR=0x41E`. The write now uses
   `DISTRIBUTOR_BASE + GICD_IPRIORITYR + TIMER_IRQ`.
4. **The syscall message ABI is `#[repr(C)]` on both sides.** Kernel and
   userspace `Message` layouts must match. `repr(Rust)` let the compiler
   reorder fields, so userspace could interpret payload bytes as the sender.
5. **SVC decoding preserves live user registers.** `exception_sync_entry`
   saves x9/x10 before reading ESR and restores them before dispatch.
   Optimized userspace may keep live values in any register across `svc #0`.
6. **Fault stack dumps are bounds checked.** Each 8-byte word is accepted by
   `mmu::valid_user_slice()` before it is read; an unmapped word prints
   `<unmapped>` instead of causing a secondary EL1 data abort.

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

For bare-metal profiles, `build.rs` invokes Cargo in `userspace/`, uses
`target/userspace` as its target directory, matches debug/release profile and
copies the resulting ELF into `OUT_DIR`. `src/user_program.rs` embeds those
bytes with `include_bytes!`. Host builds skip userspace.

QEMU run: `timeout 5s qemu-system-aarch64 -machine virt -cpu cortex-a72
-m 128M -nographic -kernel target/aarch64-unknown-none/debug/silver_microkernel`.
Exit code `124` = the kernel lived the whole time = success.

## Hardware contract (QEMU virt)

- RAM: physical `0x40000000`, 128 MB (defined in `memory.rs` and `-m 128M`)
- UART PL011: physical `0x09000000`; GICv2 Distributor `0x08000000`,
  CPU Interface `0x08010000`. Drivers access them through the high alias.
- Timer: ARM Generic Timer, IRQ 30, 100 Hz tick (`timer::TICK_INTERVAL_MS = 10`)
- FP/SIMD enabled in `_start` (CPACR_EL1 = 0x300000, CPTR_EL2 = 0) —
  without this the first use of a float register traps with EC=7
- Runtime is EL1h for the kernel and EL0t for user tasks: QEMU `-kernel`
  starts at EL2, `_start` drops to EL1 via ERET (HCR_EL2.RW=1,
  SPSR_EL2=0x3C5). VBAR_EL1, both TTBRs, TCR_EL1, MAIR_EL1 and SCTLR_EL1
  are programmed before normal Rust subsystem initialization.
- Translation uses 48-bit VAs on both halves (T0SZ=T1SZ=16), 4 KB granules,
  40-bit PAs and four table levels. MMIO is device-nGnRnE; DRAM is normal
  WB inner-shareable. Data/instruction caches remain off (SCTLR.C/I = 0).

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

The five statically allocated boot tables have fixed 4 KB slots:

| Index | Table | Role |
| --- | --- | --- |
| 0 | low L0 | Temporary boot TTBR0; emptied after the high-PC transition |
| 1 | high L0 | TTBR1 root |
| 2 | shared L1 | MMIO gigabyte at index 0, DRAM gigabyte at index 1 |
| 3 | MMIO L2 | 512 × 2 MB EL1-only device blocks |
| 4 | DRAM L2 | 512 × 2 MB EL1-only normal-memory blocks |

For privileged code, physical address `p` is `0xFFFF000000000000 + p` via
TTBR1. `memory::current()` converts high linker symbols back to PAs so the
frame allocator remains physical-address based. Kernel heap and kernel-stack
virtual addresses, including SP_EL1, are high; allocator logs may separately
show the physical heap base.

Each task owns eight pages of TTBR0 tables plus its image and data pages:

- L0[0] → user L1; L1[1] → user L2; L2[0] → a task-owned image L3.
  The image L3 maps the freshly loaded ELF pages at `0x40000000`.
- L2[511] → a task-owned L3 mapping two stack pages at fixed VA
  `0x7FE00000` (8 KB total). Equal stack VAs have different physical backing.
- L0[1] → a private L1/L2/L3 chain mapping one writable 4 KB page at
  `0x0000008000000000`.
- There are no user mappings for kernel text/rodata, MMIO, kernel `.data` or
  `.bss`, kernel heap, kernel stacks, unrelated DRAM or page-table pages.
- `destroy_task_space()` frees all task-owned tables and the private page;
  `Task::release_space()` then releases the independently owned ELF frames.

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
and UXN/PXN; data, stack and private memory are RW at EL0 and UXN/PXN. Each
spawn loads another copy, so even writable ELF data/BSS is private. VM A/B
write `0xAAAA` / `0xBBBB` to both their private page and `ELF_BSS_CELL` and
read back only their own values.

## EL0 execution model

- `scheduler::spawn_inner(entry, user)` creates a kernel stack and, for user
  tasks, an 8 KB user stack, a private page and an independently loaded ELF
  image before constructing TTBR0. Only idle remains an EL1 task.
- A fresh user context has high SP_EL1, the ELF entry in x19, and SP_EL0 =
  `0x7FE00000 + 0x2000`. It resumes at high `silver_user_bootstrap`; the
  bootstrap sets ELR_EL1 = x19, SPSR_EL1 = 0 (EL0t, IRQs unmasked), and
  executes `eret`.
- The scheduler switches only TTBR0_EL1, then performs
  `tlbi vmalle1; dsb ish; isb`. High kernel text and SP_EL1 remain valid
  through TTBR1 throughout the switch.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`; EL1 is a
  kernel panic.
- User tasks use wrappers in `userspace/src/syscall.rs`; nameserver itself is
  an EL0 task using IPC and print syscalls. Syscalls include sleep, send,
  receive, register, lookup, print, whoami, hex/dec print, mutex lock/unlock,
  and channel send/receive.
- User tasks cannot use the high-only kernel heap. The nameserver therefore
  stores 16 fixed-capacity entries on its user stack instead of in a `Vec`.
- `valid_task_slice()` delegates to `mmu::valid_user_slice()`: buffers are
  limited to 4096 bytes and every covered page must be present in the current
  TTBR0. Validation walks actual tables rather than assuming linker ranges.
  It does not yet check read/write permissions or provide a full copyin API.

**Temporary demo dispatch:** all roles share one `silver_userspace.elf`.
`userspace/src/main.rs` calls `sys_whoami` and selects nameserver, demos or
probes by fixed task ID. Keep this table synchronized with `scheduler::init`.
This is deliberately not presented as a general spawn-with-arguments API.

## Page-table rules learned the hard way

- User ELF text is AP=0b11 (read-only at EL1 and EL0), executable at EL0 and
  PXN. User rodata is read-only and UXN/PXN.
- User data, stacks and private pages are AP=0b01 and UXN/PXN. TTBR1 MMIO and
  DRAM mappings are AP=0b00 and absent from TTBR0.
- AP=0b10 is not user read-only: with the two-range EL1/EL0 regime it means
  read-only at EL1 and no EL0 access. Use AP=0b11 for user-readable rodata.

**Critical ARM64 rule:** when the translation regime has two address ranges,
a page writable at EL0 (AP=0b01) is execute-never at EL1, regardless of PXN.
QEMU implements this rule in its page-table walk. Therefore executing kernel
trampolines from an EL0-RW mapping produces an instruction abort. Kernel
execution remains in TTBR1; user text is read-only and PXN.

PAN is currently disabled in the boot stub and `configure_and_enable`: SVC
handlers directly read and write current-task buffers through TTBR0. Revisit
this when proper copyin/copyout accessors are implemented.

## Architectural invariants (do not break!)

1. **Single-CPU.** Every `unsafe impl Sync` on an UnsafeCell wrapper
   (`TaskManager`, `IpcState`) rests on a single CPU core. Real spinlocks
   come before SMP.
2. **Critical sections via DAIF.** Global kernel state (scheduler, IPC) is
   mutated only under `save_and_disable_irq` / `restore_irq`. That is the
   only mutual-exclusion mechanism besides Mutex + Condvar.
3. **The DAIF rule for tasks.** A voluntary context switch (`sleep` /
   `yield` / `block_current`) inherits masked IRQs. Kernel task entry points
   must call `interrupts::enable_irq()` before waiting; idle does this.
   User tasks get unmasked PSTATE from `silver_user_bootstrap` and from the
   SPSR saved in their exception frames.
4. **IRQ handler order** (`gic::silver_handle_irq`):
   `timer::handle_irq()` → `end_interrupt()` (EOI) → `scheduler::schedule()`.
   EOI strictly before the switch.
5. **The heap is initialized after the frame allocator** and only through
   `alloc_contiguous_pages`. No `alloc` before `allocator::init()`.
6. **Context switch layout:** x19-x30 at offsets 0-95, SP_EL1 at 96,
   SP_EL0 at 104, resume PC (`TaskContext.elr`) at 112 — 120 bytes total.
   `silver_context_switch` saves/restores this block and finishes with
   `ret`. On save, x30 is copied into the resume-PC slot; on restore, the
   resume-PC slot wins over the saved x30. Fresh tasks put their entry or
   `silver_user_bootstrap` there.
7. **Task states:** `Ready / Running / Sleeping(deadline_ticks) /
   Blocked / Dead`. The scheduler gives the CPU only to Ready tasks;
   sleeping and blocked tasks are skipped; `find_next_task` prefers
   non-idle tasks.
8. **Both trampolines keep ELR/SPSR on the stack.** Each builds a 272-byte
   frame: x0-x30, ELR at offset 248, SPSR at offset 256. The epilogue
   restores ELR/SPSR from the frame before `eret`. They mask IRQs on entry
   and do not need to re-arm ELR/SPSR for scheduling: the ret-based switch
   unwinds back to the epilogue with the original SP. CPU-global ELR/SPSR
   may be clobbered by other tasks in between; the frame copy is authoritative.
9. **SVC register preservation starts before ESR decoding.** x9/x10 are user
   registers, not free scratch. The sync entry saves/restores them around its
   dispatch check; the full 272-byte frame is built afterwards.
10. **Nameserver holds the well-known ID NAMESERVER_ID = 1**, fixed by spawn
    order in `scheduler::init` (first after idle). Protocol: text over mailbox
    messages, "R <name>" → "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING".
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

IPC state lives behind `Mutex<IpcState>` and every mailbox owns a `Condvar`.
`Condvar::wait` releases the mutex before blocking, and senders wake only the
waiters of the addressed mailbox. Check the condition and `wait` under the
lock; `notify_*` under the lock; re-check after waking.

IRQ masking guards the tiniest scheduler-critical sections; Mutex + Condvar
guard everything that can wait on a condition.

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

- One embedded ELF supplies all demos through fixed-ID role dispatch. There
  is no user-facing spawn/exit, argument passing, executable selection or
  general process lifecycle API.
- The loader supports only static `ET_EXEC`, a contiguous 2 MB image window
  and page-uniform permissions; there is no PIE, dynamic linking, demand
  loading or sparse multi-window image layout.
- Pointer validation checks mapped pages and a 4096-byte length cap, but not
  access permissions. There are no general copyin/copyout APIs.
- PAN is off because kernel code directly accesses user pages through TTBR0.
- A task that dies while sitting in a mutex or condvar queue leaves a stale
  waiter entry. Nameserver unregister and ownership cleanup are missing.
- Mutex queues, condvar queues and mailboxes have no growth limits.
- `receive()` has no timeout.
- Task address spaces hold one private page and one fixed-size 8 KB stack;
  there is no per-task heap or growable user mapping.
- UART lines from different tasks can interleave; an IRQ may even schedule
  another task during a FIFO wait.
- `#[alloc_error_handler]` is unstable on stable Rust — instead a wrapper
  in `allocator.rs` catches the null from `GlobalAlloc` and raises a panic.
- `Condvar::notify_all` is currently unused and produces a dead-code warning.

## Next steps (by priority)

1. **Task lifecycle syscalls:** user `spawn`/`exit`, spawn arguments and
   executable selection, waiter cleanup on exit, nameserver unregister and
   reliable resource reaping. Replace fixed-ID demo role dispatch.
2. **Checked copyin/copyout and PAN:** stop dereferencing user pointers
   directly in arbitrary kernel code, validate permissions, then re-evaluate
   SCTLR.PAN.
3. **Grow per-task spaces:** multiple private pages, per-task heap region,
   sparse image mappings and demand paging later.
4. **IPC hardening:** bounded mailboxes/backpressure, receive timeouts,
   nameserver watch notifications instead of lookup polling.
5. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.
6. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

## Workflow for a new feature

1. Code → `cargo fmt --all` and `cargo fmt --manifest-path userspace/Cargo.toml`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. Also check `--release`: optimized userspace exposes register-preservation
   bugs that debug builds can hide.
5. QEMU run for 5–30 s, compare output against expectations; repeat several
   times for scheduler/interrupt changes because races are timing-sensitive.
   Expect exactly the two intentional faults from tasks 15 and 16.
6. Update the README checklist and this file.
