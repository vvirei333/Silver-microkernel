# Entry point 1 — SVC: EL0 → kernel

Audited code snapshot: `snapshot/` in this directory (generated from commit
`1b0f52d`; `src/` is identical to release `dc1dcd7`). Line numbers below refer
to those copies.

## The path

1. **Vector table** — `snapshot/main-vectors.rs`, `main.rs:1050-1086`.
   Four exception groups (current EL with SP_EL0, current EL with SP_ELx,
   lower EL AArch64, lower EL AArch32), each 0x800 apart; every group routes
   sync → `exception_sync_entry`, IRQ → `exception_irq_entry`, FIQ/SERROR →
   `exception_dispatch`.
2. **`exception_sync_entry`** — `main.rs:1087`. Masks IRQs (`msr daifset,
   #2`), saves x9/x10/x11 (still live userspace values), reads `ESR_EL1`,
   checks `EC == 0b010101` (SVC from AArch64). Any other sync exception from
   EL0 goes to `exception_user_fault`; from EL1 it is a kernel bug and panics
   through the normal path.
3. **`exception_svc_dispatch`** — `main.rs:1205`. Builds the syscall frame on
   the *kernel* stack:

   | offset | contents |
   |---|---|
   | 0..240 | x0-x29 (16 bytes per pair) |
   | 240 | x30 (LR) |
   | 248 | `ELR_EL1` — return address after `svc` |
   | 256 | `SPSR_EL1` — caller PSTATE |

   272 bytes total, 16-byte aligned. `FRAME_QUADWORDS = 33` (`syscall.rs:136`),
   so `frame[31]` is ELR and `frame[32]` is SPSR. ELR/SPSR live in the frame,
   not in the CPU registers, because a blocking syscall switches tasks and
   another task's exception would overwrite the global `ELR_EL1`/`SPSR_EL1`.
   The handler saves the EL0 FP/SIMD bank (`silver_fp_save`) and calls
   `silver_handle_syscall(frame)`.
4. **`silver_handle_syscall`** — `syscall.rs:213`. Takes the global kernel
   lock (`BklGuard::new()`), stores the user PSTATE in the `USER_PSR`
   diagnostic, reads `frame[8]` (x8 = call number) and calls
   `dispatch_for_personality`. The result goes into `frame[0]`. For the Linux
   personality a pending signal is delivered on the way out (after `frame[0]`
   is set, so the signal number can replace it).
5. **`dispatch_for_personality`** — `syscall.rs:244`. Linux tasks branch to
   `linux_abi::dispatch` (`linux_abi.rs:184`); native tasks match the number
   against the Silver table. The final arm is `_ => SYSCALL_ERROR`
   (`syscall.rs:424`).
6. **Epilogue** — `main.rs:1241`. Restores FP/SIMD, publishes `ELR_EL1` and
   `SPSR_EL1` from the frame, reloads x0-x30 and `eret`s. A context switch
   resumes the task via `ret` into `schedule()`, so SP still points at this
   frame and the epilogue always finds its slots.

## What the kernel trusts, and what it must not

- `x0-x5`: arbitrary 64-bit values. Every handler must treat them as hostile:
  validate pointers, clamp lengths, reject out-of-range ids.
- `x8`: only selects a `match` arm; an unknown number returns
  `SYSCALL_ERROR` (`u64::MAX`), it must never index a table or panic.
- `ELR`/`SPSR`: written by hardware at entry and kept in the kernel stack
  frame. Userspace cannot write them.
- The kernel stack frame: owned by the kernel; the SVC path only reads the
  register bank it saved itself.

## Checklist for the CVE map

- **S1 unknown/negative call numbers**: every dispatch layer ends in a
  catch-all; no arm may index a table with `x8` or with an unvalidated arg.
- **S2 user pointers**: all dereferences of task memory must pass through
  `mmu::valid_user_slice[_mut]` and the `ldtrb`/`sttrb` copy helpers
  (`user_memory.rs:18,80`). PAN stays enabled; anything using
  `core::slice::from_raw_parts` on a user address is a finding.
- **S3 check-then-use across blocking**: the documented rule is that
  validation and the final copy run in one BKL-protected non-blocking
  critical section; if the syscall blocks in between, it must revalidate
  after the wake. `sys_waitpid` (`syscall.rs:469-500`) spells the rule out and
  is the reference implementation.
- **S4 frame indexing**: only `0..32`; `frame[32] & 0xF` gates signal
  delivery to EL0t contexts (`syscall.rs:190-210`).
- **S5 capability checks**: per call, and global rights only through
  self-capabilities (`capability.rs:99-140`). `grant_capability`
  (`scheduler.rs:4698`) must not turn a spawn handle into a global right.
- **S6 bounded tables**: fd tables, task ids, capability tables
  (`MAX_CAPS_PER_TASK = 96`), mailbox queues — every user-controlled value
  that indexes kernel storage needs a bounds check at the syscall boundary.
- **S7 locking**: no spinlock may be held across a blocking call; the BKL is
  released by `schedule_released()` and re-acquired on resume.
- **S8 return path**: publishing ELR/SPSR before restoring x0-x30, and
  restoring FP/SIMD last, must stay ordered (an interrupt between them must
  not expose a half-restored frame).

## Specific leads worth a first pass

- **Q1 — `copy_*_user` chunking.** One validation window is capped at 4096
  bytes (`mmu.rs:3156`). `copy_from_user`/`copy_to_user` split at 4096 bytes,
  but an unaligned 4096-byte chunk spans two pages; confirm
  `valid_user_access` walks both (`current_user_page_access` loop) and that a
  page straddle cannot validate only the first page.
- **Q2 — concurrent `munmap`.** A sibling thread (CLONE_VM) could unmap the
  receive buffer after validation. The design answer is the global BKL: while
  a syscall runs, no other task enters the kernel, and only the current task
  can change its TTBR0. Verify no copy path releases the BKL between the
  check and the store (the `back_user_pages` fault-repair path is the
  interesting one).
- **Q3 — kernel stack depth.** Four usable pages + one unmapped guard
  (`kernel_stack.rs:10-33`). The 272-byte frame plus VFS/ELF/network call
  depth must stay under that; an overflow hits the guard and panics (DoS,
  not corruption, but still a bug class).
- **Q4 — `USER_PSR`.** `smp::USER_PSR.store_volatile(frame[32] as u32)` is a
  global diagnostic written by every task; harmless only if nothing makes a
  security decision from it.
- **Q5 — Linux personality.** `linux_abi::dispatch` (`linux_abi.rs:184`) is
  the second dispatcher on the same trampoline: check its catch-all,
  argument truncation (32-bit calls vs 64-bit registers), `ERESTARTSYS`
  restart semantics and the signal-frame validation (`linux_signal.rs`).

## Out of scope for this file

IRQ → kernel, task faults (COW, permission), and the Linux syscall bodies are
separate entry points; they are referenced here only where the SVC path hands
control to them.
