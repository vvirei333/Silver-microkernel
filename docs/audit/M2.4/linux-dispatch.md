# Entry point 5 (tails) — Linux dispatcher, signals, fd lifetime

Audited code: `snapshot/linux_signal.rs`, `linux_fd.rs`, `linux_elf.rs`, and
`linux-dispatch.rs` (dispatcher head, altstack, `kill`/`tgkill`). The M2.2
packet already covered `dispatch`'s argument/errno contract and the L1/L2
findings; this pass closes the deferred L3-L9 leads on the shipped tree.

## The path

- **`dispatch`**: `frame[8]` is the syscall number, `frame[0..6]` are
  `x0..x5`. The match returns either a value or `-errno` (`errno(code)`);
  `finish(Result)` maps the fd layer's positive codes. Unknown numbers return
  `-ENOSYS` (optionally traced, capped at 48 lines).
- **Signals**: dispositions live in the **thread-group** table (`owner()` is
  `group_leader`), pending/blocked/altstack in the **per-task** table.
  `deliver_pending` runs on return to EL0 (SVC epilogue and the IRQ hook),
  picks the first deliverable `pending & !blocked`, builds a 128-byte
  `siginfo` + 4544-byte `ucontext` frame, and redirects the task to the
  handler with the restorer in LR. `restore_frame` (`rt_sigreturn`) reads the
  ucontext back, sanitises PSTATE (L1), restores registers/SP/PC/mask and
  clears `SS_ONSTACK`.
- **`kill`/`tgkill`**: `may_signal` allows self, same thread group,
  parent/child, an explicit `CAP_KILL` handle, or another Linux task; native
  daemons stay unreachable without a handle. `SIGKILL` and default-terminating
  signals exit/kill directly; a real handler records pending and wakes the
  target.
- **fds**: `linux_fd` holds a per-task table; `fork_table` clones slots and
  `retain_endpoint()`s pipes/memfds, `close_task` drops the table and
  `release_endpoint()`s each slot. VFS handles are duplicated by
  `vfs::dup_owner` under the same numbers, so the table and the VFS agree.

## Trust boundary

Every pointer is a user VA copied through `user_memory::{copy_from,to}_user`;
every length/offset is checked before use. The ucontext is user memory but
only its own task's context is restored, and PSTATE is gated. Signal numbers
are clamped to 1..=64. Caller identity is the scheduler's current task id.

## Leads for this packet

- **L3 — `pc`/`sp`/regs from the ucontext.** With PSTATE forced to AArch64
  EL0t these are the task's own context. Verify that an EL0 fault at a kernel
  VA (or an unmapped user VA) kills the task and never panics the kernel:
  `silver_handle_user_fault` → `cow_fault` → false → `silver_handle_task_fault`
  → `exit_current_with_status` (M2.3 snapshot `exceptions.rs` / `fault-path.md`).
- **L4 — allocation per signal.** `deliver_pending` allocates
  `SIGFRAME_SIZE = 4672` bytes and a 4544-byte ucontext buffer under the BKL
  for every delivered signal; OOM is the F5 class (allocator halts). Pending
  is a bitmask, so a storm coalesces per signal, but a handler that re-raises
  itself is a CPU/allocator amplifier. Check the delivery loop cannot recurse:
  `deliver_pending` is called once per EL0 return, not from inside itself.
- **L5 — altstack.** `sigaltstack` accepts `size >= MINSIGSTKSZ (2048)`,
  rejects `SS_ONSTACK`, `sp == 0` and `sp + size` overflow. Delivery uses the
  alt stack only when `size >= SIGFRAME_SIZE`, aligns the frame down to 16
  bytes, and checks `signal_sp >= stack_floor && end <= stack_top`, killing
  with SIGSEGV otherwise. Note: an unaligned `sp` with `size` exactly
  `SIGFRAME_SIZE` can align the frame one 16-byte slot below `sp` and take the
  SIGSEGV path — fail-closed, not a write outside the range. `SS_ONSTACK` is
  cleared by `sigreturn` and by `set_altstack(None)`.
- **L6 — dispositions vs threads.** Dispositions are keyed by
  `group_leader(task)`; pending/blocked are per task. `deliver_pending` masks
  with the *thread* blocked mask and the *group* action. Check `tgkill`
  targeting (it routes through `linux_kill`, whose `may_signal` accepts the
  same group), and that `close_task` removes only the dying task's table while
  the group leader's dispositions survive until the leader exits.
- **L7 — `rt_sigaction` flags.** `SA_RESTORER` with a user restorer > 1 is
  accepted (it runs at EL0); unknown flags are stored but never interpreted.
  Check that no unknown flag can turn a terminating `SIG_DFL` into a
  no-op — `deliver_pending` looks only at `handler`, and `SIG_DFL` either
  ignores (non-terminating) or `terminate`s. `SA_NODEFER` and `SA_RESETHAND`
  are handled; `SA_ONSTACK` only selects the alt stack.
- **L8 — invoking `rt_sigreturn` directly.** A task may call it with a
  crafted SP and no handler. It validates `sp + SIGINFO_SIZE` (overflow →
  SIGSEGV), copies the ucontext with `copy_from_user` (fault → SIGSEGV),
  sanitises PSTATE (non-EL0t → SIGSEGV), and then restores *its own* registers
  and mask. `set_blocked` strips SIGKILL/SIGSTOP. Confirm nothing outside the
  ucontext (e.g. a saved kernel pointer or an altstack flag) is trusted.
- **L9 — fd table lifetime.** `close_range`/`dup*`/`fork`/`exec` sharing and
  the M0.2 class (a descriptor remembered by number instead of a refcount)
  are the next pass. The kernel SCM_RIGHTS path is covered by
  `tests/security/fd_lifetime.c`; `linux_fd::fork_table` retains endpoints and
  `close_task` releases them, so check `dup2` over a live target, `close_range`
  with `CLOSE_RANGE_CLOEXEC`, and `execve` replacing the table without leaking
  the old endpoints.
