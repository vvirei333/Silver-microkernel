# Entry point 4 — IRQ: device interrupt → userspace driver

Audited code snapshot: `snapshot/` (current `main`). This is the third way
into the kernel, and the one that carries device DMA.

## The path

1. **Vector** — `main.rs:1245-1270` routes IRQ from any EL to
   `exception_irq_entry`, which branches to `exception_irq_dispatch`
   (`main.rs:1272`).
2. **`exception_irq_dispatch`** — masks IRQs, builds the same 272-byte frame
   as the SVC trampoline (x0-x30, ELR, SPSR), saves the EL0 FP/SIMD bank only
   when the interrupted context was EL0, then calls `silver_handle_irq()`.
   On the way out `silver_irq_signal_return` may deliver a pending Linux
   signal, and the common epilogue restores ELR/SPSR and `eret`s.
3. **`silver_handle_irq`** — `gic.rs:350` dispatches on the platform:
   GICv3 → `gicv3::handle_irq` (`gicv3.rs:711`), else the GICv2 path in
   `gic.rs`.
4. **Device INTID** (both controllers):
   - take the BKL, `scheduler::irq_is_bound(intid)`;
   - if bound: `disable_irq`, EOI, `scheduler::wake_bound_irq(intid)` —
     sets `irq_pending` on the owning task and wakes it;
   - if not bound: `mask_irq`, EOI, `note_unknown_irq`; after a bounded streak
     the kernel stops (`silver_handle_exception(8, …)`).
5. **`SYS_BIND_IRQ`** — `scheduler::bind_irq` (`scheduler.rs:4978`): the INTID
   must pass `irq_policy::is_user_bindable` (SPI only, never the timer), the
   task needs a **self** `CAP_IRQ`, no other live task may own the INTID, then
   the binding is stored, `irq_pending` cleared and the line enabled.
6. **`SYS_WAIT_IRQ`** — `scheduler::wait_irq` (`scheduler.rs:1812`): with the
   line enabled, block until `irq_pending`; each wake re-enables the line and
   returns one event.
7. **Teardown** — `exit_current_with_status` (`scheduler.rs:2054`) and
   `kill_task` clear `bound_irq`/`irq_pending` and free the rest of the task's
   state. The handler had already disabled the line when it woke the task.

## Why it is shaped this way

- The probe (`irq_is_bound`) and the wake are one BKL-protected step. Split
  into two windows, another CPU could unbind the INTID in between; the line
  would then be masked and nobody woken — a silently lost interrupt.
- SGIs and PPIs are never user-bindable: they are banked per PE and include
  the reschedule SGI, the quiesce SGI and the timer PPI. `is_user_bindable`
  is host-tested policy (`irq_policy.rs`), not an MMIO helper.
- The line is disabled before EOI and re-enabled only by `wait_irq`/`bind_irq`.
  That is the driver's backpressure: one pending event at a time.

## Leads for this packet

- **R1 — unknown-INTID halt as a user-triggerable panic?** A task can bind an
  SPI, receive one interrupt, and exit (or be killed) before draining. The
  next interrupt finds `irq_is_bound == false`, masks the line and feeds
  `note_unknown_irq`. A level-driven source is masked after one event, so the
  streak counter should not advance; confirm the same for edge sources and
  that no user action can produce eight *unmaskable* events
  (`note_unknown_irq` returns false only when the mask cannot be proven).
- **R2 — `wait_irq` loop.** It re-enables the line before blocking; a spurious
  wake re-checks `irq_pending` and blocks again. Verify a task cannot spin
  burning CPU without ever blocking (e.g. `irq_pending` set by a second CPU
  between the check and the block), and that `WAIT_IRQ` on an unbound INTID
  returns `SYSCALL_ERROR` without enabling anything.
- **R3 — bind/unbind races.** `bind_irq` checks exclusivity and stores the
  binding under the BKL; `kill`/`exit` clear it under the BKL. Check the
  ordering against `wake_bound_irq` (which runs under the BKL in the handler)
  and against `irq_policy` changes at runtime (none today).
- **R4 — GICv2/GICv3 policy drift.** The two handlers are separate code paths
  with the same rules. Compare them: EOI ordering, spurious handling
  (1023), reserved INTIDs, and whether either can enable an INTID that
  `is_user_bindable` rejects.
- **R5 — `exit_current_with_status`.** It clamps the status, takes the BKL
  only if absent (the fault path enters without it), writes
  `clear_child_tid` through `copy_to_user`, then runs the teardown
  (`ipc::remove_task`, fd/signal/futex/VFS close, console recovery,
  reparenting). Check the ordering for a task dying while blocked in IPC or
  holding a mutex, and that a failure in one cleanup cannot skip the rest
  (the code is a sequence of independent calls — verify each is
  failure-tolerant).
- **R6 — timer path scheduling.** The timer handler EOI-then-schedules when
  `irq_may_schedule()`; the reschedule SGI does the same. Check the
  interaction with `BklGuard` and with a task that is inside a syscall
  (`irq_may_schedule` must be false while the BKL is held by this CPU).
- **R7 — IRQ during `eret`/signal delivery.** `silver_irq_signal_return`
  runs with the frame on the stack; it checks `SPSR & 0xF` (EL0 only) before
  delivering. Verify a signal delivered from an EL1 interrupt (kernel code)
  cannot mutate the kernel frame — the check should be exactly the PSTATE
  gate, same as L1.
