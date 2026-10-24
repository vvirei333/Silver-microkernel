# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24 (night)

Latest verified feature: **the kernel runs in the TTBR1 high half while EL0
tasks receive minimal per-task TTBR0 mappings**. Kernel data, heap, kernel
stacks, page tables and MMIO are absent from user TTBR0. All demo tasks except
idle still run at EL0t and enter the kernel only through `svc #0`.

Verified with host and bare-metal builds, a 20-second QEMU run, five
consecutive 12-second runs, and a final 12-second smoke run. Every run had
exactly one intentional `TASK FAULT` from the isolation probe and no
`KERNEL PANIC`, `BAD SP`, or `ISOLATION FAIL`. IPC, nameserver, mutex/channel,
condvar and the VM A/B private-page demo continued after that task died.

The probe reads `0xFFFF000040000000` from EL0 and reports:

```text
TASK FAULT
ID=15
EC=0000000000000024
FAR=FFFF000040000000
SP_EL0=000000007FE01FC0
```

EC `0x24` is a data abort from a lower exception level: the expected
permission/translation failure, not a kernel regression. Task IDs depend on
spawn order; the current order makes the isolation probe ID 15.

Three important fixes from the EL0/high-half work must be preserved:

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

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

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

The six statically allocated boot tables have fixed 4 KB slots:

| Index | Table | Role |
| --- | --- | --- |
| 0 | low L0 | Temporary boot TTBR0; emptied after the high-PC transition |
| 1 | high L0 | TTBR1 root |
| 2 | shared L1 | MMIO gigabyte at index 0, DRAM gigabyte at index 1 |
| 3 | MMIO L2 | 512 × 2 MB EL1-only device blocks |
| 4 | DRAM L2 | 512 × 2 MB EL1-only normal-memory blocks |
| 5 | user image L3 | Shared read-only low `.text` / `.rodata` alias |

For privileged code, physical address `p` is `0xFFFF000000000000 + p` via
TTBR1. `memory::current()` converts high linker symbols back to PAs so the
frame allocator remains physical-address based. Kernel heap and kernel-stack
virtual addresses, including SP_EL1, are high; allocator logs may separately
show the physical heap base.

Each task owns seven pages of TTBR0 tables plus its data pages:

- L0[0] → user L1; L1[1] → user L2; L2[0] → the shared read-only image L3.
- L2[511] → a task-owned L3 mapping two stack pages at fixed VA
  `0x7FE00000` (8 KB total). Equal stack VAs have different physical backing.
- L0[1] → a private L1/L2/L3 chain mapping one writable 4 KB page at
  `0x0000008000000000`.
- There are no user mappings for MMIO, `.data`, `.bss`, kernel heap, kernel
  stacks, other DRAM, or page-table pages. The shared image L3 is never
  freed by `destroy_task_space()`; task-owned tables and pages are freed.

**Remaining compatibility alias:** built-in user functions are still linked
into the kernel ELF, so `.text` and `.rodata` remain visible read-only at low
addresses in every user TTBR0. This is not writable kernel-state exposure,
but it is still shared kernel image visibility. Separate user ELF binaries
are required to remove it; do not claim TTBR0 is completely empty of kernel
image pages yet.

## EL0 execution model

- `scheduler::spawn_inner(entry, user)` creates a kernel stack and, for user
  tasks, a separate 8 KB user stack before constructing TTBR0. Only idle
  remains an EL1 task.
- A fresh user context has high SP_EL1, low user entry
  `virt_to_phys(entry)`, and SP_EL0 = `0x7FE00000 + 0x2000`. It resumes at
  high `silver_user_bootstrap` with x19 = low entry; the bootstrap sets
  ELR_EL1 = x19, SPSR_EL1 = 0 (EL0t, IRQs unmasked), and executes `eret`.
- The scheduler switches only TTBR0_EL1, then performs
  `tlbi vmalle1; dsb ish; isb`. High kernel text and SP_EL1 remain valid
  through TTBR1 throughout the switch.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`; EL1 is a
  kernel panic.
- User tasks use wrappers in `syscall.rs`; nameserver itself is an EL0 task
  using IPC and print syscalls. Syscalls include sleep, send, receive,
  register, lookup, print, whoami, hex/dec print, mutex lock/unlock, and
  channel send/receive.
- User tasks cannot use the high-only kernel heap. The nameserver therefore
  stores 16 fixed-capacity entries on its user stack instead of in a `Vec`.
- `valid_task_slice()` delegates to `mmu::valid_user_slice()`: buffers are
  limited to 4096 bytes and must fit inside the shared image, current fixed-VA
  stack, or private page. This is range validation, not a full copyin API.

## Page-table rules learned the hard way

- Shared low `.text` is AP=0b11 (read-only at EL1 and EL0), executable at
  both levels. Shared `.rodata` is read-only and UXN/PXN.
- User stacks and private pages are AP=0b01 and UXN/PXN. TTBR1 MMIO and DRAM
  mappings are AP=0b00 and absent from TTBR0.

**Critical ARM64 rule:** when the translation regime has two address ranges,
a page writable at EL0 (AP=0b01) is execute-never at EL1, regardless of PXN.
QEMU implements this rule in its page-table walk. Therefore executing kernel
trampolines from an EL0-RW `.text` mapping produces an instruction abort.
Making `.text` read-only avoids that EL0-writable condition and preserves
EL1 execution. Do not mark user-writable pages as kernel-executable.

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

- **The built-in read-only image alias remains.** Writable privileged state
  and MMIO are isolated through TTBR1, but every user TTBR0 can still read
  the statically linked `.text` / `.rodata`. Separate user ELF binaries are
  needed before each task can receive only its own executable image.
- Pointer validation checks the current task's fixed user ranges and a
  4096-byte length cap. There are no general copyin/copyout APIs.
- PAN is off because kernel code directly accesses user pages through TTBR0.
- A task that dies while sitting in a mutex or condvar queue leaves a stale
  waiter entry. There are no user-facing spawn/exit syscalls yet.
- Mutex queues, condvar queues and mailboxes have no growth limits.
- `receive()` has no timeout.
- Task address spaces hold one private page and one fixed-size 8 KB stack;
  there is no per-task heap or growable user mapping.
- UART lines from different tasks can interleave; an IRQ may even schedule
  another task during a FIFO wait.
- `#[alloc_error_handler]` is unstable on stable Rust — instead a wrapper
  in `allocator.rs` catches the null from `GlobalAlloc` and raises a panic.

## Next steps (by priority)

1. **ELF loader and real user programs:** load separate binaries, construct
   per-task executable mappings, and remove the shared low image alias.
2. **Task lifecycle syscalls:** user `spawn`/`exit`, waiter cleanup on exit,
   nameserver unregister, and reliable resource reaping.
3. **Checked copyin/copyout and PAN:** stop dereferencing user pointers
   directly in arbitrary kernel code, then re-evaluate SCTLR.PAN.
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
