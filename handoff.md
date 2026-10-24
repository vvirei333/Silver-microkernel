# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24

Latest verified feature: **MMU bring-up**. The kernel now runs at EL1
(QEMU's `-kernel` starts at EL2 — `_start` drops to EL1h via ERET, with
HCR_EL2.RW=1) with translation enabled: identity map of the low MMIO
gigabyte (device nGnRnE) and the DRAM gigabyte (normal WB), 4 KB granule,
four-level tables with 2 MB blocks at level 2, AP=EL1-RW. Log prints
`MMU=OK`; every demo (nameserver, ping-pong over SVC, condvar test, channel
backpressure, mutex, sleeper) works unchanged on top.
Verification: `cargo build` (host) + bare-metal build + 6 seconds in QEMU
with stable output (see README).

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
- Runtime is always EL1h: QEMU `-kernel` starts at EL2, `_start` drops to
  EL1 via ERET (HCR_EL2.RW=1, SPSR_EL2=0x3C5). All system registers are the
  EL1 variants; VBAR_EL1, TTBR0_EL1, TCR_EL1, MAIR_EL1, SCTLR_EL1
- MMU on from the first line of kernel_main: identity map, 4 KB granule,
  4-level tables in `.bss` (`mmu::TABLES`), 2 MB blocks at L2. MMIO gigabyte
  = device nGnRnE, DRAM gigabyte = normal WB inner-shareable, AP=EL1-RW
- Data/instruction caches stay OFF for now (SCTLR.C/I = 0) — only
  translation is on

## Architectural invariants (do not break!)

1. **Single-CPU.** Every `unsafe impl Sync` on an UnsafeCell wrapper
   (`TaskManager`, `IpcState`) rests on a single CPU core. Real spinlocks
   come before SMP.
2. **Critical sections via DAIF.** Global kernel state (scheduler, IPC) is
   mutated only under `save_and_disable_irq` / `restore_irq`. That is the
   only mutual-exclusion mechanism.
3. **The DAIF rule for tasks.** A voluntary context switch (`sleep` /
   `yield` / `block_current`) inherits masked IRQs. Every task entry point
   must call `interrupts::enable_irq()` before it can be woken by the
   timer again. Forgetting it produces a "frozen" kernel that is actually
   sleeping with IRQ masked (we already shipped this bug once — see git
   history).
4. **IRQ handler order** (`gic::silver_handle_irq`):
   `timer::handle_irq()` → `end_interrupt()` (EOI) → `scheduler::schedule()`.
   EOI strictly before the switch.
5. **The heap is initialized after the frame allocator** and only through
   `alloc_contiguous_pages`. No `alloc` before `allocator::init()` (no
   `Vec` in statics touched before heap initialization).
6. **Context switch** (`silver_context_switch`) saves x19–x30 + SP,
   104 bytes. The IRQ trampoline in `main.rs` saves the full x0–x30 bank.
7. **Task states:** `Ready / Running / Sleeping(deadline_ticks) /
   Blocked / Dead`. The scheduler gives the CPU only to Ready tasks;
   sleeping and blocked tasks are skipped; `find_next_task` prefers
   non-idle tasks.
8. **SVC trampoline keeps ELR/SPSR on the stack.** The sync exception
   entry checks ESR EC: `0b010101` (SVC from AArch64) goes to
   `exception_svc_dispatch`, everything else panics as before. The
   trampoline saves x0-x30 + ELR_EL1 + SPSR_EL1 in a 272-byte stack frame
   and restores ELR/SPSR *from the stack* before eret — never trusting the
   CPU registers, which are global per-CPU and get overwritten when the
   task is switched away mid-syscall (all syscalls may block).
   Calling convention: number in x8, args in x0-x5, result in x0.
   The table lives in `syscall.rs` (`silver_handle_syscall`).
9. **Nameserver holds the well-known ID NAMESERVER_ID = 1**, fixed purely by
   spawn order in `scheduler::init` (first after idle). The syscall clients
   (register/lookup RPC) hardcode this ID — that is the one hardcode that is
   allowed to stay. Protocol is text over mailbox messages: "R <name>" →
   "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING" (see nameserver.rs).
10. **TLB/cache ritual for any future table edit.** Translation tables are
    CPU-read memory: after modifying them you must clean the lines to the
    point of coherency (`dc cvac` + `dsb`), invalidate stale TLB entries
    (`tlbi` + `dsb` + `isb`). Template: `mmu::flush_after_table_change`.
    First enable is exempt (cold TLB, caches off) but follows the same code.

## Why IPC uses Mutex + Condvar (and what changed)

Early IPC used IRQ-masked global state with a global `receivers` list, because
blocking on the raw mutex while holding it would deadlock the sender. The
condition variable removes exactly that restriction: `Condvar::wait` releases
the mutex *before* blocking, so IPC state now lives behind
`Mutex<IpcState>` and every mailbox owns a `Condvar`. Senders wake only the
waiters of the addressed mailbox — targeted wakeups, no spurious scheduling.
Rule of thumb, updated: IRQ masking guards the tiniest scheduler-critical
sections; Mutex + Condvar guard everything that can wait on a condition.

Condvar discipline (same as std): check the condition and `wait` under the
lock; `notify_*` under the lock; re-check the condition after waking.

## Known tech debt

- A task that dies (`exit_current`) while sitting in someone else's mutex
  or condvar queue leaves a stale waiter entry (leak, and a future
  wake_task on a dead ID is a harmless no-op). No demo task currently does
  this — none of them ever exit.
- Mutex queues, condvar queues and mailboxes have no growth limits.
- `receive()` has no timeout: a task can wait for a message forever.
- No MMU: tasks share one address space; stacks are protected only by
  care (`debug_assert` in `schedule` checks SP within stack bounds).
- Tasks run at EL1 and are linked into the kernel image; syscall arguments
  are trusted (raw pointers are dereferenced without validation). Moving
  tasks to EL0 + MMU isolation makes this exploitable surface real.
- UART printing is not atomic: lines from different tasks can interleave
  byte by byte. Add a pl011 spinlock (task context only!).
- `#[alloc_error_handler]` is unstable on stable Rust — instead a wrapper
  in `allocator.rs` catches the null from `GlobalAlloc` and raises a
  KERNEL PANIC.

## Next steps (by priority)

1. **More syscalls behind the SVC boundary:** `spawn`, `exit`, mutex
   lock/unlock, UART write — so tasks stop calling kernel code directly.
2. **Nameserver extras:** unregister on task exit, duplicate-name policy
   (replace vs TAKEN), watch/notifications for "task appeared" instead of
   lookup polling.
3. **IPC on top of the channel:** mailbox payloads could ride a bounded
   channel per task to get backpressure on message floods for free.
4. **Per-task address spaces:** dedicated L3 tables per task (stacks and
   heap already come from the frame allocator), TTBR0 switch on context
   switch, kernel mapped into every task at a high VA, then move tasks to
   **EL0** (separate SP_EL0, SVC from lower EL). The two together are the
   real isolation milestone.
5. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.
6. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

## Workflow for a new feature

1. Code → `cargo fmt --all`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. QEMU run for 4–5 s, compare output against expectations
5. Update the README checklist and this file
