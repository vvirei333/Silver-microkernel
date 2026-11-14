# Entry point 2 — IPC: mailbox send/receive

Audited code snapshot: `snapshot/` (commit `1b0f52d`; `src/` identical to
release `dc1dcd7`). Line numbers refer to those copies.

## The path

| Call | Number | Handler |
|---|---|---|
| `SYS_SEND` | 2 | `sys_send_impl(target, ptr, false)` — `syscall.rs:1290` |
| `SYS_TRY_SEND` | 65 | `sys_send_impl(target, ptr, true)` |
| `SYS_RECEIVE` | 3 | `sys_receive(buffer_ptr, timeout_ticks)` — `syscall.rs:1318` |

**Message format** (`ipc.rs:17-27`): `sender: u64` + `payload: [u8; 32]`,
`repr(C)`, 40 bytes. The kernel copies exactly 40 bytes to/from the user.

**Send** — `sys_send_impl`:
1. `copy_from_user(message_ptr, &mut message)` — the whole 40-byte struct is
   read before anything else.
2. `message.sender = scheduler::current_task_id()` — identity is a kernel
   fact; a user-supplied `sender` field is overwritten (anti-spoofing, and it
   is what makes `receive_from` trustworthy).
3. `ipc::send(target, message)` or `ipc::try_send`.

**`ipc::send`** (`ipc.rs:81`): under the global IPC mutex, loop:
`task_alive(target)`? → `mailbox_index()` (create on first use) → if the
queue has fewer than `MAILBOX_CAPACITY = 256` messages, push and
`not_empty.notify_one()`; otherwise wait on the mailbox's `not_full`
condition (an `Arc<Condvar>` so the queue can be freed while a sender is
blocked). A dead target returns `TargetDead` before and after every wake.

**`ipc::try_send`** (`ipc.rs:102`): same checks, but a full queue returns
`QueueFull` instead of blocking (drivers use this to drop input rather than
stall their IRQ loop).

**Receive** — `sys_receive`:
1. `mmu::valid_user_slice_mut(buffer_ptr, 40)` *before* blocking, so a bad
   pointer fails fast instead of sleeping.
2. `ipc::receive_timeout(timeout_ticks)`.
3. `copy_to_user(message, buffer_ptr)` — the copy helper validates again
   after the wake.

**`ipc::receive_matching`** (`ipc.rs:132`): under the IPC mutex, look up the
caller's own mailbox; remove the first message (optionally filtered by
`sender`); `not_full.notify_one()`; if nothing matched and a sender filter is
set, check the sender is alive (`TargetDead`); enforce the absolute deadline
(`timer::ticks()`), then wait on `not_empty`. `INFINITE_TIMEOUT = u64::MAX`,
`0` polls.

**Death cleanup** (`ipc::remove_task`, `ipc.rs:184`): clears the dead owner's
queue, broadcasts on *every* mailbox's two conditions (so a task blocked in
`receive_from(dead)` wakes and re-checks), then drops the owner's mailbox.
`Vec`/`VecDeque` memory for queues is therefore bounded by the live task
count.

**Nameserver RPC** (`sys_register_name` 1345, `sys_lookup_name` 1387):
`read_name` copies a NUL-terminated name bounded by `NAME_LIMIT`, packs
`"R <name>"` / `"L <name>"` into one message, `ipc::send(SCOUT_ID)` then
`ipc::receive_from(SCOUT_ID)`, and parses `"OK"` / `"ID <digits>"`.

## Trust rules as written

- **Sender identity** cannot be forged: the kernel overwrites `sender`.
- **Target** is a raw task id (`u64`). There is **no capability check** on
  `SYS_SEND`: any task may send to any live task. Mailboxes are ambient
  authority keyed by pid.
- **Payload** is 32 opaque bytes; the kernel never interprets it (the
  nameserver protocol is a user-space convention).
- **Queue** is per-mailbox, 256 messages, FIFO, blocking send with
  backpressure, `try_send` drops.
- **Receive buffer** is 40 bytes, validated before blocking and revalidated by
  the copy-out.

## Checklist for the CVE map

- **I1 target authorization / spam.** Any task can fill another task's
  mailbox (256 × 40 B) and keep it full, forcing the receiver to drain or
  making a sender block. There is no capability or quota per (sender,
  target) pair. Decide whether ambient send-by-pid is intended for M2 or
  whether send should require a name/capability handle.
- **I2 name squatting / first-caller-trusts-last.** `R`/`L` are
  unauthenticated and handled by Scout, itself an EL0 task. Can a task
  register a name another task will later look up and receive its traffic?
  Check Scout's registration policy (`userspace/src/bin/scout.rs`) for
  duplicate/owner-replacement rules.
- **I3 lost wakeup in `Condvar::wait`.** `wait` drops the caller's mutex
  (`ipc.rs`'s `IPC`) *before* `scheduler::block_current_on` enqueues the
  waiter. That is only safe because the task still holds the global BKL until
  `schedule_released()`, and `notify_*` needs the BKL to run. Verify no path
  releases the BKL between `drop(guard)` and `enqueue`, and that
  `wake_task` cannot observe a task that is not yet in the queue.
- **I4 lazy mailbox creation vs death.** `send` checks `task_alive` and then
  `mailbox_index` creates a mailbox for the target. If the target dies
  between the check and the creation (or after `remove_task` ran), can a
  mailbox for a dead task survive until the id is reused? It is bounded by
  the task table, but a leak/stale reply is still a protocol break.
- **I5 allocation in the kernel path.** `push_back` may grow the `VecDeque`;
  under memory pressure the kernel allocator's failure mode must be a
  returned error, not an abort/panic. The global 16 MiB heap is shared with
  every task.
- **I6 memory accounting.** Queue memory (up to 64 × 256 × 40 B ≈ 640 KiB) is
  not charged to the sender's memory ledger; a task can allocate kernel heap
  by sending. Compare with the per-task `mem_frames` ledger.
- **I7 timeout arithmetic.** `deadline = ticks().saturating_add(timeout)` and
  `INFINITE_TIMEOUT = u64::MAX`; `0` polls. Confirm `wait_until` can never
  sleep past a deadline due to a wrapped/woken state, and that a timed wait
  removes itself from the queue (`block_current_on_until` does).
- **I8 unbounded reply wait.** `receive_from(SCOUT_ID)` has no timeout: a
  live but hung Scout wedges every task that registers or looks up a name.
  Only `TargetDead` breaks the wait. Decide whether the RPC needs a deadline
  (and what the caller sees on timeout).
- **I9 reply parsing.** `Message::text()` truncates at the first NUL;
  `parse_id_reply` requires `"ID "` + ASCII digits with `checked_mul/add`.
  Check the `"OK"` path and that a hostile Scout cannot return a task id that
  grants anything by itself (it should not — ids are not capabilities).
- **I10 queue cleanup.** `remove_task` broadcasts while holding the IPC
  mutex; confirm a woken waiter cannot re-enter `mailbox_index(id)` for the
  dead id and recreate the mailbox (the `task_alive` check on send prevents
  this for senders; receivers only use their own id).
