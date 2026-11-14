# Entry point 5 — EL0 faults and COW

Audited code snapshot: `snapshot/` (current `main`). This is how a task that
touches memory it does not own enters the kernel, and the only path that can
*resume* the faulting instruction.

## The path

1. **Vector** — `exception_sync_entry` (`main-fault.rs`): ESR.EC != SVC and
   SPSR says EL0 → `exception_user_fault`. It saves x0-x30 + ELR/SPSR into the
   272-byte frame and the EL0 FP/SIMD bank, then calls
   `silver_handle_user_fault(frame, esr, elr, far)`.
2. **`silver_handle_user_fault`** (`exceptions.rs:48`, Linux ABI only) takes
   the BKL and calls `scheduler::cow_fault(far, esr)`.
   - `true` → return 0; the trampoline leaves ELR unchanged and `eret`s back
     into the faulting instruction, which is retried.
   - `false` → `silver_handle_task_fault(esr, elr, far)`: prints
     `Task N killed: Page Fault at 0x…`, dumps the switch trace and calls
     `exit_current()` (never returns).
   Without the Linux ABI the symbol is a direct task kill (no COW pages).
3. **`cow_fault`** (`scheduler.rs:6647`, excerpt):
   - Only a **write** fault (ESR.WnR, bit 6) can be a COW repair; a read fault
     falls through to the kill.
   - **Lazy mmap regions** are backed on first access (read or write):
     `lazy_page_fault(index, page, esr)`.
   - **Linux ELF image runs** marked `Cow`/`Data` are copied into a fresh
     contiguous run, remapped, the old run freed, and the kinds updated.
   - The **user stack** is copied and remapped writable.
   - Any other address, or a failed allocation/remap, kills the task.

`fork_current` (`scheduler.rs:3972`) is the producer of the COW state: it
clones the address space with the writable runs marked `Cow`; `cow_copy_run`
allocates a new run and copies the old pages, generation-tagged so the frame
allocator can detect a stale free.

## Trust boundary

The fault address (`FAR_EL1`) and syndrome (`ESR_EL1`) are hardware facts, not
user arguments; the frame (registers, ELR) is kernel-saved. The dangerous
inputs are the **region metadata** the user built (`mmap`/`mprotect`/`brk`
lengths and offsets) and the **frame allocator state** shared by fork.

## Leads for this packet

- **C1 — retry fidelity.** On `true` the ELR/SPSR are restored from the frame
  and the instruction retried. Check that IRQ/FIQ masking and the FP bank are
  restored exactly (the vector saves FP only for EL0), and that a fault which
  repairs one page but would also fault the next byte can make forward
  progress (no livelock).
- **C2 — rollback on partial COW.** `cow_fault` frees the new run when the
  remap fails, but the old run must stay mapped and charged. Verify every
  early return after `cow_copy_run` frees the new generation and does not touch
  the old, and that a double fault on the same page cannot free a shared run
  twice (generation check).
- **C3 — lazy region arithmetic.** `lazy_page_fault` indexes a region by
  `(page - base)/PAGE`; check `virtual_address + page_count*PAGE` cannot wrap
  (a user-controlled `mmap` length near `usize::MAX`), and that the index is
  bounded by the region's page count.
- **C4 — fork/COW frame accounting.** `fork_current` shares frames with
  `Cow` kinds; the per-task ledger is charged once or twice? Check that the
  child's ledger and the parent's release path cannot free a frame still
  referenced by the other (the generation tag is the guard).
- **C5 — fault while holding a lock.** The dying task may hold a mutex or a
  filesystem lock; `silver_handle_task_fault` deliberately does no allocation
  or locking, and `exit_current` runs `release_mutexes`. Verify every owned
  lock (mutex, VFS, IPC) is released on the kill path, including when the
  fault happens inside a syscall that had already taken the BKL (reentrant
  BklGuard).
- **C6 — interrupt state on the retry path.** The vector masks IRQs; the
  epilogue restores PSTATE from the frame. Check that a fault taken inside a
  syscall (same EL1 frame) cannot happen — the vector routes EL1 sync
  exceptions to `silver_handle_exception` and halts — and that a user fault
  cannot leave IRQs permanently masked on this CPU.
- **C7 — `mprotect`/`mmap` metadata.** The region lists are built from user
  arguments; check length/offset arithmetic (`checked_*`), overlap handling,
  and that `mprotect` cannot turn a shared/file mapping into a writable COW
  page with a stale physical frame.
- **C8 — Linux fault semantics.** A faulting Linux task is `exit_current()`'d
  (status 0) instead of receiving SIGSEGV. Check whether that is intended for
  0.1.3 (crash reporting/`waitpid` in Phosh expects a signal). This is a
  behavioural gap, not a memory-safety one.
