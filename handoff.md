# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24 (night)

Latest verified feature: **all demo tasks except idle run in EL0 (Ring 3)**.
Each user task has a separate SP_EL0 stack and reaches the kernel only through
`svc #0`. The syscall layer validates task pointers before dereferencing them;
UART/GIC MMIO is inaccessible at EL0. A non-SVC exception originating at EL0
prints `TASK FAULT` and terminates only that task through `exit_current()`.

Verified with host and bare-metal builds, ten consecutive 12-second QEMU runs,
and a 40-second run: no `TASK FAULT`, `KERNEL PANIC`, or `BAD SP`. The VM
isolation demo kept alternating `0xAAAA` / `0xBBBB`; the condvar test reported
`CONDVAR TEST: beta silent = PASS`.

Two important fixes landed while stabilizing EL0:

1. **The context switch now resumes with `ret`, not `eret`.** The former
   eret-based switch saved SP deep inside `schedule()` but resumed directly
   in a trampoline epilogue. That epilogue expects SP to point at its
   272-byte exception frame, so it read garbage as ELR and could eret to
   EL0 at addresses such as 0x2. With the ret-based switch, the saved x30
   returns into `schedule()`, the original call chain unwinds back to the
   trampoline, and the epilogue naturally has the correct SP.
2. **UART TX FIFO waits no longer freeze the kernel.** The old PL011 driver
   spun on TXFF with IRQs masked inside syscall handlers. When the host did
   not drain the UART quickly enough, the timer could not preempt the spin
   and the whole system appeared hung. `write_byte()` now temporarily
   unmasks IRQs only while waiting for FIFO space and restores the caller's
   DAIF afterwards.

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

QEMU run: `timeout 5s qemu-system-aarch64 -machine virt -cpu cortex-a72
-m 128M -nographic -kernel target/aarch64-unknown-none/debug/silver_microkernel`.
Exit code `124` = the kernel lived the whole time = success.

## Hardware contract (QEMU virt)

- RAM: 0x40000000, 128 MB (defined in `memory.rs` and `-m 128M`)
- UART PL011: 0x09000000
- GICv2: Distributor 0x08000000, CPU Interface 0x08010000
- Timer: ARM Generic Timer, IRQ 30, 100 Hz tick (`timer::TICK_INTERVAL_MS = 10`)
- FP/SIMD enabled in `_start` (CPACR_EL1 = 0x300000, CPTR_EL2 = 0) —
  without this the first use of a float register traps with EC=7
- Runtime is EL1h for the kernel and EL0t for user tasks: QEMU `-kernel`
  starts at EL2, `_start` drops to EL1 via ERET (HCR_EL2.RW=1,
  SPSR_EL2=0x3C5). All system registers are the EL1 variants; VBAR_EL1,
  TTBR0_EL1, TCR_EL1, MAIR_EL1, SCTLR_EL1
- MMU on from the first line of kernel_main: identity map, 4 KB granule,
  4-level tables, 2 MB blocks at L2. MMIO gigabyte = device nGnRnE,
  DRAM gigabyte = normal WB inner-shareable
- Data/instruction caches stay OFF for now (SCTLR.C/I = 0) — only
  translation is on

## EL0 execution model

- `scheduler::spawn_inner(entry, user)` creates both a kernel stack and,
  for user tasks, a user stack. Only idle remains an EL1 task.
- A fresh user context resumes at `silver_user_bootstrap` with x19 = task
  entry and SP_EL0 already loaded. The bootstrap sets ELR_EL1 = x19,
  SPSR_EL1 = 0 (EL0t, IRQs unmasked), and executes `eret`.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`; EL1 is a
  kernel panic.
- User tasks do not call `pl011`, scheduler, mutex, IPC, or nameserver code
  directly. They use wrappers in `syscall.rs`; nameserver itself is an EL0
  task using IPC and print syscalls.
- Syscalls currently include sleep, send, receive, register, lookup, print,
  whoami, hex/dec print, mutex lock/unlock, and channel send/receive.
- `valid_task_slice()` currently accepts bounded buffers inside the DRAM
  identity range only. It rejects MMIO and the private VA window because
  those addresses are not available to the kernel's current mapping.

## Page-table rules learned the hard way

Task tables retain a shared identity-mapped kernel half plus per-task
private mappings. They are not a naive copy of the kernel's original RW
mappings:

- `.text` is AP=0b11 (read-only at EL1 and EL0), executable at both levels.
- `.rodata` is read-only and UXN/PXN.
- `.data`, `.bss`, and stacks are AP=0b01 and UXN/PXN.
- Other DRAM blocks are AP=0b01 with UXN/PXN; the private user page is
  AP=0b01 with UXN.
- MMIO is AP=0b00, so EL0 cannot touch UART or GIC directly.

**Critical ARM64 rule:** when the translation regime has two address ranges,
a page writable at EL0 (AP=0b01) is execute-never at EL1, regardless of PXN.
QEMU implements this rule in its page-table walk. Therefore executing kernel
trampolines from an EL0-RW `.text` mapping produces an instruction abort.
Making `.text` read-only avoids that EL0-writable condition and preserves
EL1 execution. Do not mark user-writable pages as kernel-executable.

PAN is currently disabled in `configure_and_enable`: the kernel deliberately
reads and writes user task buffers. Revisit this when proper copyin/copyout
accessors and stronger isolation are designed.

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
9. **Nameserver holds the well-known ID NAMESERVER_ID = 1**, fixed by spawn
   order in `scheduler::init` (first after idle). Protocol: text over mailbox
   messages, "R <name>" → "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING".
10. **TLB/cache ritual for any future table edit.** After modifying tables,
    clean lines to the point of coherency (`dc cvac` + `dsb`), invalidate
    stale TLB entries (`tlbi` + `dsb` + `isb`). Template:
    `mmu::flush_after_table_change`. Address-space switches load TTBR0_EL1
    and issue `tlbi vmalle1; dsb ish; isb` before the context switch.
11. **UART FIFO waits preserve DAIF.** `pl011::write_byte()` may unmask IRQs
    while waiting for TXFF, but restores the caller's saved DAIF before
    returning. Never replace this with an indefinitely masked spin. Taking
    a timer IRQ mid-print is intentional and safe with the frame-based
    trampolines.

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
until task lifecycle and isolation are more battle-tested.

QEMU exception tracing is invaluable for resume/fault bugs:

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel \
  -d int -D qemu-int.log
```

## Known tech debt

- **Kernel data is not isolated yet.** EL0 tasks can still access shared
  `.data`, `.bss`, kernel stacks, and other AP=0b01 DRAM mappings. `.text`
  and `.rodata` are read-only and MMIO is blocked, but this is not full
  process isolation. Move the kernel to TTBR1/high-half mappings or remove
  kernel data from user TTBR0 tables.
- Pointer validation checks only the DRAM range and length, not ownership
  of a particular task's memory. There are no general copyin/copyout APIs.
- PAN is off because kernel code directly accesses user pages.
- A task that dies while sitting in a mutex or condvar queue leaves a stale
  waiter entry. There are no user-facing spawn/exit syscalls yet.
- Mutex queues, condvar queues and mailboxes have no growth limits.
- `receive()` has no timeout.
- Task address spaces hold exactly one private page; there is no per-task
  heap or growable user mapping.
- UART lines from different tasks can interleave; an IRQ may even schedule
  another task during a FIFO wait.
- `#[alloc_error_handler]` is unstable on stable Rust — instead a wrapper
  in `allocator.rs` catches the null from `GlobalAlloc` and raises a panic.

## Next steps (by priority)

1. **Complete user/kernel isolation:** move kernel mappings out of user
   TTBR0 (high-half/TTBR1 design), give tasks only their own code/data,
   and introduce checked copyin/copyout. Re-evaluate PAN at the same time.
2. **Task lifecycle syscalls:** user `spawn`/`exit`, waiter cleanup on exit,
   nameserver unregister, and reaping user resources.
3. **ELF loader and real user programs:** load separate binaries instead of
   linking demo tasks into the kernel image.
4. **Grow per-task spaces:** multiple private pages, per-task heap region,
   demand paging later.
5. **IPC hardening:** bounded mailboxes/backpressure, receive timeouts,
   nameserver watch notifications instead of lookup polling.
6. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.
7. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

## Workflow for a new feature

1. Code → `cargo fmt --all`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. QEMU run for 5–30 s, compare output against expectations; repeat several
   times for scheduler/interrupt changes because races are timing-sensitive
5. Update the README checklist and this file
