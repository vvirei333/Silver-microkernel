# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-24

Latest verified feature: **system calls (SVC)**. The full IPC ping-pong demo
now goes through `svc #0` — including the blocking `receive`, which is the
real regression test: a task blocks *inside a syscall*, gets switched away,
and eret still returns to the right place.
Verification: `cargo build` (host) + bare-metal build + 5 seconds in QEMU
with stable output (see README). Running simultaneously: the mutex demo,
the IPC ping-pong over SVC (period ~300 ms) and the sleep task (500 ms).

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

## Why IPC is not built on mutex::Mutex

The receiver registers itself in the wait list and blocks as one atomic
operation (under the IRQ mask). If IPC state were protected by the blocking
mutex, the receiver would fall asleep holding the very lock the sender needs
to deliver — an instant deadlock. Rule of thumb: the blocking mutex is for
task data; IRQ masking is for kernel state.

## Known tech debt

- `wake_task()` wakes any Blocked task — there is no notion of *why* it
  blocked. Spurious wakeups are safe (the task just re-checks its
  condition), but the semantics are coarse. When condvars/waitqueues
  arrive, replace the global `receivers` list with per-channel queues.
- A task that dies (`exit_current`) while sitting in someone else's mutex
  queue or in `IpcState::receivers` leaves a stale entry (waiter leak).
  No demo task currently does this — none of them ever exit.
- Mutex queues, mailboxes and `receivers` have no growth limits.
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

1. **Nameserver / task registry.** Right now ping hardcodes pong's ID = 5.
   Need names or handles handed out at spawn time (itself a nice IPC
   exercise: register/lookup via messages to a nameserver task).
2. **Condvar / wait queue** on top of the mutex: `wait(cond)` / `notify()`,
   per-channel queues instead of the global `receivers`.
3. **More syscalls behind the SVC boundary:** `spawn`, `exit`, mutex
   lock/unlock, UART write — so tasks stop calling kernel code directly.
4. **Tasks at EL0** (AArch64 "Ring 3"): separate SP_EL0, exception-return
   discipline lower-EL, then MMU isolation becomes meaningful.
5. **MMU + TTBR0/TTBR1, page tables, stack isolation.** The biggest
   milestone on the road to a "real" microkernel.
6. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface.

## Workflow for a new feature

1. Code → `cargo fmt --all`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. QEMU run for 4–5 s, compare output against expectations
5. Update the README checklist and this file
