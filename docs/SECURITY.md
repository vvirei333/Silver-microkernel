# Security model and disclosure protocol

Status: living document, first revision for M2 (2026-11). It states what the
kernel accepts from a task, what it refuses to trust, and how a finding is
handled. It is the contract the audit is measured against; when the code and
this document disagree, the code is the bug until proven otherwise.

## Assets and trust boundaries

| Component | Trust |
|---|---|
| Kernel (scheduler, MMU, IPC, VFS/SFS, drivers) | Trusted computing base |
| Native drivers with `CAP_MMIO`/`CAP_IRQ`/`CAP_PCI` | Part of the TCB by capability, still one fault domain each |
| `silverd`, Scout, Howl | Privileged native tasks; the kernel does not trust their *input* |
| Linux ABI tasks (Phosh, GTK apps, test binaries) | Untrusted |
| Network and on-disk images | Untrusted |
| Physical access, microarchitecture (Spectre-class) | Out of scope for M2 |

The kernel must stay correct when **any** EL0 task is hostile: it may pass
arbitrary register values, unmap its own pages mid-call, send messages to any
pid, race its own threads, and lie in every payload.

## What a task may hand the kernel

### 1. The SVC register bank

A syscall is `svc #0` with the number in `x8` and arguments in `x0-x5`
(`src/main.rs` trampoline, `src/syscall.rs`). Everything in those registers is
hostile:

- `x8` only selects a dispatch arm. Unknown numbers return
  `SYSCALL_ERROR`; they never index a table and never panic.
- Pointer arguments are validated against the task's **own** TTBR0
  (`mmu::valid_user_slice[_mut]`) and copied with `ldtrb`/`sttrb` while
  `SCTLR_EL1.PAN` stays enabled. A user address is never dereferenced as a
  kernel pointer.
- Lengths and ids are clamped at the boundary (page-sized validation windows,
  `NAME_LIMIT`, capability table bounds, fd-table bounds). A handler that
  indexes kernel storage with a user value must check the bound first.
- `ELR_EL1`/`SPSR_EL1` are saved by the trampoline into the kernel stack
  frame; userspace cannot write them. The frame is 272 bytes on a 4-page
  kernel stack with an unmapped guard page.

### 2. Time

A syscall may block (IPC receive, `wait`, `read` on a socket, `SIGCHLD`
wait). The rule is:

> Validation and the final copy must run in one kernel-lock-protected,
> non-blocking critical section. If the syscall blocks in between, it must
> revalidate the user buffer after the wake.

The global kernel lock (BKL) serializes kernel execution; only the current
task can change its own address space, so a pointer validated and used
without releasing the BKL cannot be unmapped underneath the copy. The
reference implementation and its rationale are `sys_waitpid` in
`src/syscall.rs`; `tests/security/` is where violations become regressions.

**The kernel lock.** The BKL is reentrant on its owning CPU (depth counter,
`smp::bkl_acquire`). Notification paths (`Condvar::notify_*` →
`scheduler::wake_task`) take it while the caller already holds it; that nesting
is part of the contract, not an accident. The same contract is what makes
`Condvar::wait` safe: it drops the object mutex *before* enqueueing, and no
other CPU can run a notifier until `schedule_released()` gives the BKL away.
Any change that releases the BKL between the unlock and the enqueue
reintroduces a lost wakeup.

### 3. Identity

Task id, message sender, mailbox owner, fd owner and console owner are facts
the kernel establishes (`scheduler::current_task_id()`), never fields a caller
supplies. `SYS_SEND` overwrites `Message::sender` after copying the payload;
a forged sender is ignored.

### 4. Authority

Authority is a capability, not an id (`src/capability.rs`):

- Target-scoped rights (`CAP_KILL`, `CAP_WAIT`, `CAP_CONSOLE_GRANT`,
  `CAP_RESUME`) apply to one task and are delegated with a handle.
- Process-wide rights (`CAP_MMIO`, `CAP_IRQ`, `CAP_PCI`, `CAP_MOUNT`,
  `CAP_POWER`, `CAP_FS_WRITE`) only count as **self**-capabilities. A spawn
  handle over a child carries `CAP_ALL` but is not an own privilege, so it
  cannot pass device, mount or power rights on.
- Delegation is all-or-nothing and bounded (`MAX_CAPS_PER_TASK = 96`); a
  mixed global|scoped request through a handle grants neither. Authority only
  flows down: the recipient must be the caller itself or a task the caller
  spawned (`SYS_CAP_GRANT` checks `ppid`).
- `CAP_DMA_SHARE` is target-scoped to the task that owns a DMA window. The
  net driver and the network clients receive it from silverd; the shell
  delegates it to Wolfser via `SYS_CAP_FIND`. Holding `CAP_MMIO` alone does
  not map another task's window.
- Task ids and mailbox targets are **not** capabilities. `SYS_SEND` is
  ambient authority by pid today; see "Known gaps".

### 5. Bounded resources

Mailboxes (256 messages × 40 B per task), capability tables, fd tables, DMA
allocations, address-space mappings and the kernel heap are bounded, and the
per-task memory ledger charges what a task can pin. Allocation failure must
surface as an error code, never as a kernel abort driven by user input.

### 6. Failure containment

- Bad pointer, dead target, full queue, unknown call: error code, no panic.
- A kernel fault at EL1 (guard page, bad kernel pointer) is a **kernel bug**;
  the machine panics by design rather than continuing with corrupt state. A
  user-triggerable path to it is a denial-of-service finding, and still a bug.
- A faulting EL0 task is killed without taking the kernel down
  (`exception_user_fault`); COW faults are repaired and the instruction is
  retried.

## Known gaps and accepted risks

These are accepted for the M2 boundary (0.1.2) and re-evaluated at every
milestone; each has an owner decision above or in the findings table.

1. **Send-by-pid is ambient authority.** Any task can send to any live pid;
   there is no send capability. A single sender is now bounded to 64 unread
   messages per target (`QueueFull` instead of blocking), so it can no longer
   pin every legitimate client; the address remains ambient by design until a
   capability-handle IPC model lands in M3.
2. **DMA sharing is per owner, not per region.** `CAP_DMA_SHARE` names the
   task whose window may be mapped; the holder can map any of that task's DMA
   regions. Region-scoped grants (and revocation) are the M3 refinement.
3. **Nameserver policy is user-space.** `R`/`L` reach Scout unauthenticated;
   name squatting/replacement depends on Scout's policy, not the kernel's.
4. **No syscall filtering.** There is no seccomp-like profile; a task may call
   every syscall it has rights for. The capability table, not the ABI, is the
   authority boundary.
5. **No KASLR.** Kernel text/data addresses are fixed. Combined with an
   arbitrary-read bug this matters; alone it is not an escape.
6. **Blocking RPC deadlines are generous.** The nameserver RPC is bounded
   (30 s), but a hung Scout still stalls each register/lookup for that long.
7. **Queue memory is not charged.** Mailbox queues live on the kernel heap
   outside the per-task ledger (bounded globally, not per sender).
8. **PID existence oracle.** `SYS_TASK_ALIVE` (and send errors) reveal whether
   an arbitrary task id is alive. Accepted for the single-user console M2.
9. **Allocation failure halts the kernel.** The global allocator's OOM path
   is a deliberate `KERNEL PANIC` (state may be inconsistent). Per-call caps
   (console write, fd/name limits) and the bounded tables keep user-driven
   allocations small, but an OOM is still a DoS, not a recoverable error.
10. **EL0 can mask IRQs (L10).** Cortex-A72 has no FEAT_FGT, so `msr
    daifset, #2` from EL0 is not trapped; a task that masks IRQs and spins
    can starve the timer on a uniprocessor. The product runs SMP (two or more
    cores in QEMU, eight on the phone target), where the other CPUs keep
    scheduling; the timer PPI is per-PE, so a second CPU still preempts and
    the task is killable. Upstream Linux has the same "don't do that" on UP;
    platforms with FGT can trap the write in M4. This is a known DoS, not a
    boundary break.
11. **Page-fault file I/O runs under the BKL (performance).** A lazy
    file-backed fault reads the page from SFS while the kernel lock is held,
    so a uniprocessor stalls every task until the read completes (on SMP it
    stalls this CPU's kernel path only). The read is bounded, checked and
    fails closed; this is a latency known-issue, not a boundary break. Moving
    the streaming read out of the lock is an M3 performance item.
12. **A dead task's mutexes are released by the reaper (R5 window).** A task
    killed while holding a mutex stays its owner until the next reap pass
    (`release_mutexes`); a waiter on that mutex is blocked for that window.
    The window is bounded and the reaper runs on the next scheduling pass,
    and the alternative (taking every lock in the dying task's context) is
    worse. Recorded as a known window, not a leak.

These are findings in their own right for the auditor; nothing in this list
is an excuse for a memory-safety bug.

## Findings fixed during M2

| ID | Finding | Class | Status |
|---|---|---|---|
| F-http | HTTP chunk size `0xFFF…F` spun `dechunk` forever | integer overflow → remote DoS | fixed (`5b01348`), fuzz regression |
| L1 | `rt_sigreturn` restored a user `PSTATE`, returning to EL1 with a user PC and registers | missing validation → local privilege escalation | fixed, `tests/security/sigreturn_pstate.c` |
| L2 | `SIGKILL`/`SIGSTOP` could be blocked | missing clamp → unkillable task | fixed |
| F4 | nameserver RPC waited forever on a live-but-hung Scout | missing timeout → wedge | fixed (30 s bound) |
| F6 | one console write could flood megabytes | unbounded input → log suppression | fixed (256 KiB/call) |
| F5 | allocation failure halts the kernel | accepted, documented (#9) | per-call caps bound reachable sizes |
| F8 | BKL reentrancy is assumed by notify paths | contract now documented | documented above |
| F9 | PID existence oracle | accepted for M2 | documented (#8) |
| F1 | send-by-pid could pin a mailbox with 256 messages | ambient authority → DoS | fixed: 64 unread per (sender,target) |
| F2 | any `CAP_MMIO` holder mapped any task's DMA | ambient authority → cross-task read | fixed: target-scoped `CAP_DMA_SHARE` (per owner; per region in M3) |
| F3 | a handle could delegate rights to an arbitrary pid | uncontrolled delegation | fixed: recipient must be self or own child |
| L10 | EL0 can `msr daifset` and starve a uniprocessor | missing trap (no FGT) | accepted: SMP product, documented (#10) |
| V-dot | the SFS backend splitter accepted `.`/`..` (the VFS normalised first) | defense in depth | fixed: backend uses `sfs_format::split_path_name`; unit test |
| R3 | a killed IRQ owner left its INTID enabled, so the next interrupt charged the unmaskable streak | missing teardown mask | fixed: the reaper disables the bound INTID before clearing it |

## Disclosure and fix protocol

1. **Do not open a public issue** for a memory-safety bug, capability bypass,
   or a user-triggerable kernel panic. Contact the maintainer (Telegram in
   `README.md`) with `file:line`, the call sequence and a reproducer.
2. A finding is reproduced as a guest test under `tests/security/` (or a host
   unit test when it is pure logic) **before** the fix. The runner is
   `tools/security-test.sh`; a fix without a failing-then-passing test is not
   accepted.
3. Severity is judged on: reachability from an untrusted task, whether it
   breaks the kernel/userspace boundary, whether it persists across tasks,
   and whether it is a DoS only. The CVE map used by the audit tracks the
   class (OOB, integer overflow, TOCTOU, lifetime/UAF, missing clamp,
   trust-of-last-caller).
4. Fixes land on `main` with the regression test and a `handoff.md` entry
   naming the class, so the same mistake is searchable later.
5. Credit is given in `RELEASE.md` unless the reporter asks otherwise.

## Checklist for every new syscall (PR gate)

- Which register arguments are pointers? Validate them with the TTBR0 helpers;
  never `from_raw_parts` a user address.
- Does the call block? If so, revalidate after the wake.
- Does it index a kernel table with a user value? Bound it.
- Which capability does it require, and is it self-scoped or target-scoped?
- Can it allocate? What happens on OOM?
- Can a hostile argument make it panic, recurse without bound, or spin?
- Add a case to `tests/security/` (or explain why the host tests cover it).
