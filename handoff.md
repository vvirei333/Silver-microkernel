# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24 (evening)

Latest verified feature: **per-task address spaces**. Every task owns a
dedicated L0 table (4 pages: L0–L3, plus one private data page), allocated
through the frame allocator at spawn and freed on reap. Each L0[0] points at
the shared identity-mapped kernel half (`identity_l1_address()`), and each
L0[1] chains to the task's own L1→L2→L3 tables mapping a private 4 KB window
at `PRIVATE_WINDOW_VA = 0x0000_0080_0000_0000`. The scheduler compares
TTBR0_EL1 values and, on change, loads the next task's table and issues
`tlbi vmalle1; dsb ish; isb` *in Rust code before* `silver_context_switch` —
safe because all kernel code and stacks live in the identity half, which is
byte-identical in every address space.

Proof: the `vm_a` / `vm_b` demo tasks both write to VA `0x0000008000000000`
(vm_a = 0xAAAA, vm_b = 0xBBBB) and read back only their own value on every
iteration; verified stable across repeated QEMU runs with zero panics.

Verification: `cargo build` (host) + bare-metal build + QEMU runs with
stable alternating `VM A`/`VM B` output (see README).

Hard-won lesson this milestone (commit c05d3b9): **ELR_EL1/SPSR_EL1 are
CPU-global.** The IRQ trampoline originally left them untouched, so an SVC
trampoline running for another task overwrote them while the interrupted
task still sat inside the IRQ trampoline — `eret` then returned into the
wrong task (BAD SP, growing idle SP, panics with garbage ELR). Fix: the IRQ
trampoline now saves/restores ELR/SPSR in its 272-byte frame exactly like
the SVC trampoline, and both trampolines mask IRQs on entry.

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
8. **Trampolines keep ELR/SPSR on the stack — both of them.** The sync
   exception entry checks ESR EC: `0b010101` (SVC from AArch64) goes to
   `exception_svc_dispatch`, everything else panics as before. The SVC
   trampoline saves x0-x30 + ELR_EL1 + SPSR_EL1 in a 272-byte stack frame
   and restores ELR/SPSR *from the stack* before eret. The IRQ trampoline
   does the same (same frame size): while a task interrupted by IRQ waits
   inside the trampoline, an SVC for another task runs and clobbers the
   CPU-global ELR/SPSR — trusting the registers there returns into the
   wrong task (heisenbug, commit c05d3b9). Both trampolines also mask IRQs
   on entry (`msr daifset,#2`).
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
- Tasks still run at EL1 and share the identity-mapped kernel half of every
  address space, so they can still read/write all of kernel memory. The
  private window is isolated; real protection only arrives with EL0 +
  removing the identity half from task tables.
- Task address spaces hold exactly one private page; there is no per-task
  heap or growable user mapping yet.
- Tasks run at EL1 and are linked into the kernel image; syscall arguments
  are trusted (raw pointers are dereferenced without validation). Moving
  tasks to EL0 + MMU isolation makes this exploitable surface real.
- UART printing is not atomic: lines from different tasks can interleave
  byte by byte. Add a pl011 spinlock (task context only!).
- `#[alloc_error_handler]` is unstable on stable Rust — instead a wrapper
  in `allocator.rs` catches the null from `GlobalAlloc` and raises a
  KERNEL PANIC.

## Next steps (by priority)

1. **Move tasks to EL0 (Ring 3):** separate SP_EL0, SVC from lower EL
   (`ESR EC=0b010101` already handles both, but EL0 needs its own stack
   switch and page-table AP bits — kernel RW only at EL1, user pages
   EL0-RW). Drop the identity half from task tables; keep the kernel
   mapped at a high VA shared by all tasks. This is the real isolation
   milestone.
2. **More syscalls behind the SVC boundary:** `spawn`, `exit`, mutex
   lock/unlock, UART write — so tasks stop calling kernel code directly.
3. **Nameserver extras:** unregister on task exit, duplicate-name policy
   (replace vs TAKEN), watch/notifications for "task appeared" instead of
   lookup polling.
4. **IPC on top of the channel:** mailbox payloads could ride a bounded
   channel per task to get backpressure on message floods for free.
5. **Grow per-task spaces:** multiple private pages, per-task heap region,
   copy-on-write later.
6. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.
7. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

## Workflow for a new feature

1. Code → `cargo fmt --all`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. QEMU run for 4–5 s, compare output against expectations
5. Update the README checklist and this file
