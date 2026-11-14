# Entry point 3 — Linux personality: dispatcher and signal frames

Audited code snapshot: `snapshot/` (current `main`). This is the second
dispatcher on the same SVC trampoline, reached when the task was spawned with
the Linux personality.

## The path

`silver_handle_syscall` (`main.rs` SVC trampoline) → `dispatch_for_personality`
→ `linux_abi::dispatch(frame)` (`linux_abi.rs:184`) →
`dispatch_inner(frame, number, args)` (`linux_abi.rs:190`).

- `number` = `frame[8]` (x8), `args` = `frame[0..6]` (x0-x5), result is
  written back to `frame[0]` by the caller.
- `finish(Result<u64, u64>)` maps an error code to its negative encoding
  (`errno(code) = -code`), so the user sees `-EFAULT`, `-EINVAL`, … exactly as
  Linux does. The catch-all arm must return `-ENOSYS`; check it.
- The personality is fixed at spawn (`Scheduler::Personality::Linux`), so a
  task cannot switch dispatchers.

## Signal machinery

| Call | Code | Kernel side |
|---|---|---|
| `rt_sigaction` | 134 | 32-byte `{handler, flags, restorer, mask}` copied in; stored per *thread group* |
| `rt_sigprocmask` | 135 | 8-byte mask; `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK` |
| `sigaltstack` | 132 | `{sp, flags, pad, size}`, 24 bytes |
| `kill` / `tgkill` | 129 / 131 | sets a pending bit in the target thread's table |
| `rt_sigreturn` | 139 | `restore_frame` reads the ucontext the kernel wrote |

`deliver_pending` (`linux_signal.rs:228`) runs on the way back to EL0:

1. `deliverable = pending & !blocked`; ignored dispositions are dropped,
   default-terminating signals exit the task, handlers get a frame.
2. The frame is `siginfo` (128 B) + `ucontext` (4544 B) built in a kernel
   `Vec`, then copied to the user stack: `signal_sp = (sp or altstack top) -
   SIGFRAME_SIZE`, 16-byte aligned, bounded by the stack window or the
   registered altstack (`size >= SIGFRAME_SIZE`, no overflow).
3. `uc_mcontext` saves x0-x30, SP, PC and **PSTATE**; `blocked | action.mask |
   signal_bit` becomes the new mask (minus `SA_NODEFER`); the handler is
   entered with `(sig, siginfo, ucontext)` and the restorer in LR.

`restore_frame` (`linux_signal.rs:348`) reads the ucontext back on
`rt_sigreturn`: registers, SP, PC, PSTATE, signal mask.

## Finding L1 — critical, fixed while preparing this packet

`restore_frame` copied `uc_mcontext.pstate` into `frame[32]` (SPSR_EL1)
without validation. The SVC epilogue does `msr spsr_el1, frame[32]` followed by
`eret`, so a handler that set `PSTATE.M = EL1h` (0x5) returned to **EL1** at
the user-supplied PC with the user-supplied x0-x30:

```
// before the fix
frame[31] = pc;
frame[32] = pstate;      // user-controlled
```

Reproducer (`tests/security/sigreturn_pstate.c`): a SIGUSR1 handler sets
`pstate = 0x5` and the instruction after the interrupting `kill` is `udf #0`.
On the unfixed kernel this produced:

```
EL1 EXCEPTION kind=0
ESR_EL1=000000008600000F      (instruction abort from EL1)
ELR_EL1=0000000000400114      (user PC, executed at EL1)
KERNEL PANIC: halting
```

The EL1 instruction abort happened only because EL1 cannot fetch from the
user half (TTBR0 walks are disabled at EL1). The PC is just as controllable:
pointing it at a kernel gadget (no KASLR) with controlled x0-x30 turns this
into an arbitrary EL1 call — local privilege escalation from any Linux task.

Fix (in the snapshot): reject a non-EL0t PSTATE exactly like Linux's
`valid_user_regs()` and kill the task with SIGSEGV.

```rust
if pstate & 0xF != 0 {
    terminate(SIGSEGV);
}
```

Regression: `sigreturn_pstate` passes (child dies with 139 / SIGSEGV, parent
prints `sigreturn pstate blocked`). The pre-fix proof is kept in the commit
message and in this file.

## Finding L2 — minor, fixed

`SIGKILL` and `SIGSTOP` could be blocked through `rt_sigprocmask` (and through
a restored sigreturn mask): the pending bit was then never delivered, so a
task could make itself unkillable. `set_blocked` now strips both bits, as
Linux does.

## Leads for this packet

- **L3 — `pc`/`sp`/regs from the ucontext.** With PSTATE forced to EL0t these
  are the task's own context. Verify that an EL0 fault at a kernel VA (or an
  unmapped user VA) kills the task and never panics the kernel.
- **L4 — allocation per signal.** `deliver_pending` allocates ~4.7 KiB under
  the BKL for every delivered signal; OOM behaviour is the F5 class (the
  kernel allocator halts). A signal storm is also a CPU amplifier.
- **L5 — altstack.** `sigaltstack` accepts any user range with
  `size >= SIGFRAME_SIZE`; check alignment, `SS_ONSTACK` bookkeeping, and that
  a frame cannot be placed outside the registered range.
- **L6 — dispositions vs threads.** Dispositions are per thread group,
  pending/blocked per thread (`owner()`), masked in `deliver_pending` with the
  *group* disposition and the *thread* mask. Check `tgkill` targeting and
  death cleanup (`close_task`).
- **L7 — `rt_sigaction` flags.** `SA_RESTORER` with a user restorer >1 is
  accepted (it runs at EL0, so fine); no other flag is validated. Check that
  unknown flags cannot suppress the default action on a terminating signal.
- **L8 — invoking `rt_sigreturn` directly.** A task may call it with a
  crafted SP and no handler; it restores its own (now EL0-only) context.
  Confirm nothing outside the ucontext is trusted.
- **L9 — fd table lifetime.** `linux_fd.rs` accompanies the snapshot:
  `close_range`, `dup*`, `fork`/`exec` sharing, and the M0.2 class (a
  descriptor remembered by number instead of refcount) are the next pass.
