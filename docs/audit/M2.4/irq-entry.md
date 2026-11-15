# Entry point 4 (tails) — IRQ delivery from GIC to a bound task

Audited code: `snapshot/irq_policy.rs`, `gic.rs`, `gicv3.rs`, `timer.rs`,
`wait_queue.rs`, plus `main-irq.rs`, `scheduler-irq.rs`, `smp-irq.rs` excerpts
and `syscall-irq-hook.rs`. This is the path that turns a device interrupt into
a woken userspace task, and the only path where a user-controlled INTID can
make the kernel touch GIC state.

## The path

1. **Vector** — `exception_vectors` (`main-irq.rs`): every IRQ slot branches
   to `exception_irq_entry` → `exception_irq_dispatch`. The trampoline saves
   x0-x30 + ELR/SPSR into a 272-byte frame, saves the EL0 FP bank only when
   `SPSR.M[3:0] == 0`, calls `silver_handle_irq`, then
   `silver_irq_signal_return(frame)` and the epilogue.
2. **`silver_handle_irq`** (`gic.rs` for GICv2, `gicv3::handle_irq` for
   GICv3; `platform::detect().uses_gicv3()` selects):
   - **Non-timer SGI/PPI/SPI:** a `BklGuard` is taken first; if
     `irq_is_bound(irq)`, the line is disabled, EOI is written, and
     `wake_bound_irq(irq)` marks `irq_pending` and wakes the owner. Probe and
     wake are one lock-protected step (a comment explains the lost-interrupt
     window that splitting them opens on SMP).
   - **Timer PPI:** `timer::handle_irq()`, EOI, then `schedule()` only when
     `smp::irq_may_schedule()` (`bkl_depth() == 0`).
   - **Reschedule SGI:** EOI first, then the same conditional `schedule()`.
   - **Quiesce SGI:** EOI, then `smp::quiesce_ack_and_wait()`.
   - **Spurious (1023):** ignored.
   - **Unknown:** mask the specific INTID in its proven scope *before* EOI,
     then `note_unknown_irq(irq, masked)`. A mask that cannot be proven feeds
     a bounded streak (`irq_policy::UNMASKABLE_HALT_LIMIT = 8`); the eighth
     calls `silver_handle_exception(8, …)`.
3. **`bind_irq`** (`scheduler-irq.rs`): `irq_policy::is_user_bindable` (SPI
   only, never the timer), `CAP_IRQ` self-right, exclusivity against live
   tasks, then store `bound_irq`, clear `irq_pending`, `gic::enable_irq`.
4. **`wait_irq`**: with IRQs masked and the BKL held, re-enable the line,
   check `irq_pending`, otherwise set `Blocked` and `schedule_released()`;
   the loop re-checks on every wake. Returns `false` immediately when the
   task has no binding.
5. **Teardown:** `bound_irq = None` in `exit_current_with_status`,
   `kill_task`, `exit_group`'s sibling kill and the reaper; `irq_pending` is
   cleared in the reaper.

## Trust boundary

The INTID comes from the GIC acknowledge register (`& 0x3FF`), not from
userspace, but a task chooses which INTID to bind and when to stop draining.
The frame is kernel-saved; `irq_pending`/`bound_irq` are scheduler state under
the BKL. The user cannot supply a handler address or a GIC register offset.

## Leads for this packet

- **R1 — unknown-INTID halt as a user-triggerable panic?** A task binds an
  SPI, receives one interrupt, exits before draining. The next interrupt finds
  `irq_is_bound == false`, masks the line and calls `note_unknown_irq`. A
  level source is masked after one event; confirm the same for edge sources
  and that no user action can produce eight *unmaskable* events. Note that a
  mask failure is only reachable for reserved/spurious INTIDs
  (`mask_scope == Unavailable`) or a GIC that does not reflect ICENABLER; the
  user cannot select those through `bind_irq`.
- **R2 — `wait_irq` loop.** It re-enables the line before blocking; a spurious
  wake re-checks `irq_pending` and blocks again. Verify a task cannot spin
  burning CPU without ever blocking (e.g. `irq_pending` set by another CPU
  between the check and the block — both sides run under the BKL, and
  `wake_bound_irq` therefore cannot interleave with the check), and that
  `WAIT_IRQ` on an unbound INTID returns `SYSCALL_ERROR` without enabling
  anything.
- **R3 — bind/unbind races.** `bind_irq` checks exclusivity and stores the
  binding under the BKL; `kill`/`exit` clear it under the BKL. Check the
  ordering against `wake_bound_irq` (which runs under the BKL in the handler)
  and against a task that is killed while `Blocked`: the reaper clears
  `irq_pending` but does not `disable_irq`; the first post-mortem interrupt is
  masked by the unknown path. Confirm that is the only fallback needed.
- **R4 — GICv2/GICv3 policy drift.** The two handlers are separate code paths
  with the same rules: bound-check under `BklGuard`, disable before EOI,
  unknown mask before EOI, timer/reschedule/quiesce handled before the bound
  branch, 1023 ignored. Compare them, including `mask_irq` (v3 uses the
  redistributor for private INTIDs; v2 uses the banked word 0) and whether
  either can enable an INTID that `is_user_bindable` rejects.
- **R5 — `exit_current_with_status`.** It clamps the status, takes the BKL
  only if absent (the fault path enters without it), writes `clear_child_tid`
  through `copy_to_user`, then runs the teardown (`ipc::remove_task`, fd /
  signal / futex / VFS close, console recovery, reparenting, waiter wake).
  Check the ordering for a task dying while blocked in IPC or holding a
  mutex, and that a failure in one cleanup cannot skip the rest (the code is a
  sequence of independent calls). Mutexes are released by the **reaper**
  (`release_mutexes`), not here; the task has already left the CPU.
- **R6 — timer path scheduling.** The timer handler EOI-then-schedules when
  `irq_may_schedule()`; the reschedule SGI does the same. Check the
  interaction with `BklGuard` and with a task that is inside a syscall
  (`irq_may_schedule` is false while this CPU holds the BKL). The timer PPI
  is per-PE, so one CPU's tick does not preempt another CPU's syscall.
- **R7 — IRQ during `eret`/signal delivery.** `silver_irq_signal_return`
  runs with the frame on the stack; it checks `frame[32] & 0x1F == 0`
  (native AArch64 EL0t only) before calling `deliver_pending` under a
  `BklGuard`. Verify a signal delivered from an EL1 interrupt (kernel code)
  cannot mutate the kernel frame, and that the gate rejects M[4] (AArch32)
  as well as the EL1 modes — the same gate as L1.
