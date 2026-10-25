# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## State as of 2026-10-25 (safe VFS rename update)

Latest verified feature: **file and directory rename/move**. `SYS_FS_RENAME`
backs the shell's `mv old new` command for absolute and cwd-relative paths. It
rejects identical source/destination paths, missing destination parents and
replacement of any existing destination. Directories cannot move inside their
own subtree, and those at or above a live task's cwd stay pinned. Open files,
including files below a moved directory, also pin the source while handles are
absolute-path based.

Earlier unlink/rmdir and per-task cwd behavior remain intact. The shell runs
ls/cat/echo as utilities and waits for them, while `echo > file`, `mkdir`, `rm`,
`rmdir`, `mv`, `cd` and `pwd` call VFS directly.

The default bare-metal boot includes only idle, nameserver and the shell, so
periodic demo output cannot bury the prompt. Build with `no_std,demo-tasks` for
the old regression suite. Verified with host, debug/release builds, scripted
QEMU UART input in both shell profiles, and a 10-second demo-tasks run. The
regression run had exactly two intentional `TASK FAULT` reports and passed:

```text
COPYOUT READONLY REJECT=PASS
COPYOUT UNMAPPED REJECT=PASS
LIFECYCLE EXIT/UNREGISTER=PASS
LIFECYCLE MUTEX CLEANUP=PASS
HEAP GROW/SHRINK=PASS
MALLOC FREE/REUSE=PASS
MALLOC BRK TRIM=PASS
GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS
GLOBAL ALLOC TRIM=PASS
IPC TIMEOUT=PASS
IPC BP SENDER RESUMED=PASS
IPC BP BACKPRESSURE=PASS
IPC EXIT WAKE=PASS
```

Debug and release QEMU checks observed `/bin`, `/etc` and `/tmp` in `ls`,
`echo 'hello' > test.txt`, `cat test.txt` printing `hello`, quoted multiword
echo, and `mkdir docs` appearing in the next listing. `kill 3` terminated a
sleeping demo and `wait 3` returned status 1; a naturally exiting echo returned
status 0 through `wait`. The cwd scenario verified `cd /bin` → `pwd` = `/bin`,
empty `/bin` in the inherited `ls`, relative file creation/cat, `cd ..`, nested
`/tmp/docs`, and `cd ../..` back to `/`. Removal tests passed in both profiles:
files and empty directories disappeared, non-empty/root/wrong-type removal was
rejected, and rmdir refused both the current directory and an ancestor of a
nested cwd before succeeding after chdir. Rename tests passed in both profiles
with absolute and relative paths: file contents survived same/cross-directory
moves, old names disappeared, existing destinations remained unchanged, and
same-path/missing-parent/self-subtree/active-cwd moves were rejected. No
`KERNEL PANIC`, `BAD SP` or unexpected `FAIL` occurred.

| Probe | Current ID | Address read from EL0 | Expected FAR |
| --- | ---: | --- | --- |
| High-half kernel | 15 | `0xFFFF000040000000` | `FFFF000040000000` |
| Removed low kernel text alias | 16 | `0x0000000040080000` | `0000000040080000` |

Both report `EC=0x24`, a data abort from a lower exception level. These are
expected translation/permission failures, not kernel regressions. IDs depend
on spawn order and are not a stable userspace API.

Important fixes and invariants from the recent work must be preserved:

1. **The context switch resumes with `ret`, not `eret`.** The saved x30
   returns into `schedule()`, the original call chain unwinds back to the
   exception trampoline, and its epilogue finds the correct 272-byte frame.
2. **UART TX FIFO waits do not freeze the kernel.** `pl011::write_byte()`
   temporarily unmasks IRQs only while waiting for FIFO space and restores
   the caller's DAIF afterwards.
3. **GIC register offsets include the distributor base.** The timer-priority
   write uses `DISTRIBUTOR_BASE + GICD_IPRIORITYR + TIMER_IRQ`.
4. **The syscall message ABI is `#[repr(C)]` on both sides.** Kernel and
   userspace `Message` layouts must match.
5. **SVC decoding preserves live user registers.** `exception_sync_entry`
   saves x9/x10 before reading ESR and restores them before dispatch.
6. **Fault stack dumps use checked copyin.** Each 8-byte word is read through
   `user_memory`; an invalid word prints `<unmapped>` instead of causing a
   secondary EL1 data abort.
7. **Exit must switch even when only idle is ready.** Reaping can reset the
   current index to zero. `next_index == current_index` is skippable only for
   a live task; a dead task still switches to idle and saves into BOOT_CONTEXT.
8. **Nameserver RPC uses selective receive.** `receive_from(NAMESERVER_ID)`
   leaves unrelated messages queued. Plain receive could consume a PING as a
   lookup reply, lose the real reply and create a persistent WHAT? loop.
9. **Mutex ownership and wait membership belong to the task.** Reaping removes
   dead waiters, releases owned mutexes and wakes the next FIFO waiter before
   freeing stacks or address spaces.
10. **`brk` growth is all-or-nothing at the API boundary.** On frame exhaustion,
    roll back every page added by the failed request and preserve the old break.
    New mappings are zeroed before becoming visible; unmap and flush before
    freeing a shrunken page.
11. **The userspace free list stays address-sorted.** Coalesce both neighbors
    before trimming. Only the final free block may shrink `brk`, and the shrink
    must succeed before unlinking it. Block sizes include the 16-byte header.
12. **A timed waiter always leaves its condition queue.** Notification pops the
    ID immediately; timeout removal happens in `block_current_on_until` after
    the task resumes. IPC checks for a matching message before checking the
    deadline, so a notify/timeout race cannot discard an available message.

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

For bare-metal profiles, `build.rs` invokes Cargo in `userspace/`, uses
`target/userspace` as its target directory, matches debug/release profile and
copies `silver_userspace`, `nameserver` and `silvershell` into `OUT_DIR`.
`src/user_program.rs` assigns program IDs 0 (demo), 1 (nameserver) and 2 (shell).
Host builds skip userspace. The default scheduler starts shell ID 2 after the
nameserver; `demo-tasks` replaces the shell boot with the regression task set.

Interactive QEMU: `qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M
-nographic -kernel target/aarch64-unknown-none/debug/silver_microkernel`.
Use Ctrl-A then X to quit. For automation, prefix `timeout 5s`; exit code 124
means the kernel lived the whole time. Scripted serial input uses LF (`help\n`);
QEMU's multiplexed stdio may consume a piped bare CR even though the shell
handles both CR and LF from UART.

## Hardware contract (QEMU virt)

- RAM: physical `0x40000000`, 128 MB (defined in `memory.rs` and `-m 128M`)
- UART PL011: physical `0x09000000`; GICv2 Distributor `0x08000000`,
  CPU Interface `0x08010000`. Drivers access them through the high alias.
- Timer: ARM Generic Timer, IRQ 30, 100 Hz tick (`timer::TICK_INTERVAL_MS = 10`)
- FP/SIMD enabled in `_start` (CPACR_EL1 = 0x300000, CPTR_EL2 = 0) —
  without this the first use of a float register traps with EC=7
- Runtime is EL1h for the kernel and EL0t for user tasks: QEMU `-kernel`
  starts at EL2, `_start` drops to EL1 via ERET (HCR_EL2.RW=1,
  SPSR_EL2=0x3C5). VBAR_EL1, both TTBRs, TCR_EL1, MAIR_EL1 and SCTLR_EL1
  are programmed before normal Rust subsystem initialization.
- Translation uses 48-bit VAs on both halves (T0SZ=T1SZ=16), 4 KB granules,
  40-bit PAs and four table levels. MMIO is device-nGnRnE; DRAM is normal
  WB inner-shareable. Data/instruction caches remain off (SCTLR.C/I = 0).

## Boot and high-half memory model

`linker.ld` links the kernel at VMA `0xFFFF000040080000`, with physical LMAs
computed as `ADDR(section) - 0xFFFF000000000000`. The ELF entry is the
physical `__boot_entry = _start - __kernel_virt_base` (`0x40080000`), so QEMU
can load and enter the image with the MMU off. Keep VMA/LMA pairs correct for
`.text`, `.rodata`, and `.data`; `.bss` and `.stack` are NOLOAD.

The early assembly stub uses only physical aliases: it sets the boot stack,
drops EL2→EL1, zeroes `.bss`, constructs boot tables, programs both TTBRs,
MAIR/TCR and SCTLR.M, then branches to the linked high PC. Only after that
branch does it install the high SP and VBAR and call `kernel_main`.
`mmu::init()` removes the temporary low identity alias from kernel TTBR0.

The five statically allocated boot tables have fixed 4 KB slots:

| Index | Table | Role |
| --- | --- | --- |
| 0 | low L0 | Temporary boot TTBR0; emptied after the high-PC transition |
| 1 | high L0 | TTBR1 root |
| 2 | shared L1 | MMIO gigabyte at index 0, DRAM gigabyte at index 1 |
| 3 | MMIO L2 | 512 × 2 MB EL1-only device blocks |
| 4 | DRAM L2 | 512 × 2 MB EL1-only normal-memory blocks |

For privileged code, physical address `p` is `0xFFFF000000000000 + p` via
TTBR1. `memory::current()` converts high linker symbols back to PAs so the
frame allocator remains physical-address based. Kernel heap and kernel-stack
virtual addresses, including SP_EL1, are high; allocator logs may separately
show the physical heap base.

Each task owns nine pages of TTBR0 tables plus its image and data pages:

- L0[0] → user L1; L1[1] → user L2; L2[0] → a task-owned image L3.
  The image L3 maps the freshly loaded ELF pages at `0x40000000`.
- L2[1] → a task-owned heap L3 covering `0x40200000..0x40400000`. The table
  starts empty and `SYS_BRK` adds/removes up to 512 data pages.
- L2[511] → a task-owned L3 mapping two stack pages at fixed VA
  `0x7FE00000` (8 KB total). Equal stack VAs have different physical backing.
- L0[1] → a private L1/L2/L3 chain mapping one writable 4 KB page at
  `0x0000008000000000`.
- There are no user mappings for kernel text/rodata, MMIO, kernel `.data` or
  `.bss`, kernel heap, kernel stacks, unrelated DRAM or page-table pages.
- `destroy_task_space()` frees all task-owned tables, heap frames and the
  private page; `Task::release_space()` then releases the ELF frames.

## ELF loading and W^X

`userspace/linker.ld` links a static `ET_EXEC` image at `0x40000000` with
separate R-X text, R-- rodata and RW data `PT_LOAD` segments. `.bss` is
`NOLOAD` and belongs to the RW segment, so its memory size can exceed its
file size.

`elf_loader::load()` currently accepts ELF64 little-endian AArch64 `ET_EXEC`
files only. It checks program-header and file bounds, requires every loadable
segment to fit within the 2 MB image window, rejects W+X segments and rejects
different permissions sharing one 4 KB page. The entry must lie in an
executable segment. It allocates fresh contiguous frames, zeroes the whole
image, copies `PT_LOAD` file bytes and records a `PageKind` for every page.

The resulting mappings enforce W^X: text is R-X at EL0 and PXN; rodata is R--
and UXN/PXN; data, heap, stack and private memory are RW at EL0 and UXN/PXN.
Each spawn loads another copy, so even writable ELF data/BSS is private. VM A/B
write `0xAAAA` / `0xBBBB` to their private page, `ELF_BSS_CELL` and heap, then
read back only their own values.

## EL0 execution and lifecycle model

- `SYS_SPAWN(program, argument)` selects program 0 (demo), 1 (nameserver) or 2
  (silvershell). `scheduler::spawn_user` creates a kernel stack, an 8 KB user
  stack, a private page and an independent ELF copy before constructing TTBR0.
  Only idle remains an EL1 task.
- A fresh user context has high SP_EL1, the ELF entry in x19, the argument in
  x20 and SP_EL0 = `0x7FE00000 + 0x2000`. `silver_user_bootstrap` moves x20 to
  x0, sets ELR_EL1 = x19 and SPSR_EL1 = 0, then executes `eret`.
- Demo roles are selected by the argument in `userspace/src/main.rs`, not by
  task ID. Nameserver, shell and utilities have separate executables in
  `userspace/src/bin/`.
- The scheduler switches only TTBR0_EL1, then performs
  `tlbi vmalle1; dsb ish; isb`. High kernel text and SP_EL1 remain valid
  through TTBR1 throughout the switch.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`; EL1 is a
  kernel panic.
- Syscalls include sleep, IPC, nameserver, print, task control, spawn/args,
  wait/kill, `brk`, UART read, uptime, meminfo and the RAM-VFS file operations.
- `SYS_READ` polls PL011 RX and returns a byte or SYSCALL_ERROR when RXFE says
  no input is available. `SYS_UPTIME` returns raw 100 Hz ticks. `SYS_MEMINFO`
  copies three u64 byte counts (total, used, free) through checked copyout;
  used includes kernel/reserved RAM as well as allocated frames.
- `SYS_TASK_LIST(buffer, capacity)` copies up to 64 `#[repr(C)]` TaskInfo entries
  (ID, numeric state, program, argument) and returns the number written. Task
  metadata lives in the Task itself, so ps can distinguish identical ELF roles.
- `SYS_SPAWN_ARGS(program, numeric, ptr, len)` copies at most 192 argument bytes
  before allocating the task. `SYS_ARGS(buffer, capacity)` returns those bytes
  to the current program and fails rather than truncating them.
- `SYS_WAIT(id)` blocks on the process wait queue and consumes one of 64 bounded
  completion records. `SYS_KILL` rejects idle/self, marks another task Dead and
  records status 1. Normal `SYS_EXIT(status)` records the supplied status.
- `SYS_SEND(target, message)` blocks while the target has 16 queued messages and
  returns `SYSCALL_ERROR` if the target is dead. Nameserver RPC paths propagate
  this failure too.
- `SYS_RECEIVE(buffer, timeout_ticks)` uses zero for a poll and `u64::MAX` for an
  infinite wait. Other values are relative 100 Hz ticks; expiry returns
  `SYSCALL_TIMEOUT = u64::MAX - 1`. Invalid buffers still return SYSCALL_ERROR.
- `SYS_BRK(0)` queries the program break. Other values set an absolute break in
  `0x40200000..0x40400000`; invalid requests and allocation failures return
  `SYSCALL_ERROR` without changing the previous break or mappings.
- Exit and kill clear the task mailbox, wake blocked senders and close every VFS
  descriptor owned by that task. Reaping removes stale waiters, releases owned
  mutexes, wakes the next waiter, then frees the ELF pages, heap frames, page
  tables, private page and both stacks. A woken sender rechecks liveness and
  cannot recreate the dead task's mailbox.
- Nameserver unregister is owner-checked. Its table is also purged through
  `task_alive` when later requests arrive, so names left by dead tasks do not
  remain permanently valid.
- User tasks cannot use the high-only kernel heap. The nameserver therefore
  stores 16 fixed-capacity entries on its user stack instead of in a `Vec`.

## RAM-VFS

- `Node` is either `File(Vec<u8>)` or `Directory(Vec<Node>)`. The volatile root
  starts with `/bin`, `/etc` and `/tmp`. Each Task stores an absolute cwd, and
  `spawn_inner` clones it into the child. Relative paths resolve from that cwd;
  `.` and `..` are normalized and `..` cannot escape root.
- Open handles store an absolute normalized path rather than a Node pointer or
  relative path, so Vec reallocation and later chdir calls cannot retarget them.
  Each handle has owner, per-owner fd (starting at 3), offset and writable flag.
  Exit/kill removes all handles for that owner.
- `SYS_FS_OPEN/READ/WRITE/CREATE/CLOSE/READ_DIR/MKDIR/UNLINK/RMDIR/RENAME` use
  checked copyin/copyout. `SYS_CHDIR` resolves and validates a directory before
  replacing the Task cwd; `SYS_GETCWD` copies its absolute bytes. Paths are
  limited to 191 bytes, names to 32 bytes and one I/O call to 4096 bytes.
  `create` truncates files, `open` supports read-only or write mode and
  `read_dir` copies 48-byte `#[repr(C)]` entries with name bytes/length and kind.
- `unlink` removes only files with no open handles; there is deliberately no
  Unix-style unlink-while-open while handles are path-based. `rmdir` removes
  only empty, non-root directories and rejects a directory used at or above any
  live task's cwd. The cwd check runs inside the same single-CPU IRQ-critical
  section as removal, so chdir/spawn cannot interleave with it.
- `rename` validates both paths and the destination parent before detaching the
  source. It never overwrites, rejects a no-op path and prevents directory
  cycles. Sources with open files below them or live cwds below directories are
  rejected instead of rewriting path-based handles/cwds. The same IRQ-critical
  section covers checks and the move, so expected failures preserve the tree.
- There is no persistence, file-size quota or append flag yet. Repeated writes
  extend the in-memory file through the kernel heap.

## silvershell

- `silvershell.rs` is an independent no_std/no_main ELF with no allocator. It
  uses a 96-byte line buffer, accepts printable ASCII and CR/LF, echoes input,
  and implements Backspace/DEL by removing the last byte and erasing it onscreen.
- UART input is deliberately a nonblocking syscall in this MVP. The shell sleeps
  for 1 ms when RX is empty, so polling does not monopolize the scheduler. A
  future UART RX interrupt can replace polling without changing command parsing.
- Startup emits ANSI clear/home, runs fastfetch once and shows `silvershell> `.
  Commands are `help`, `fastfetch`, `ls`, `cat`, `echo`, `mkdir`, `rm`, `rmdir`,
  `mv`, `cd`, `pwd`, `ps`, `spawn`, `kill` and `wait`; empty lines only reprompt.
  Echo parsing preserves quoted spaces, strips outer quotes and finds `>` outside
  quotes. `mv old new` takes a full destination path, not just a directory.
- `ls`, `cat` and ordinary `echo` spawn separate EL0 utilities with string args
  and inherited cwd, then immediately wait for them. `echo text > path`
  truncates/creates and writes directly through VFS. `mkdir`, `rm`, `rmdir`,
  `mv`, `cd` and `pwd` are also direct syscalls. Utility failures return status 1.
- `ps` uses a fixed 32-entry userspace array and prints ID/program/argument/state.
  `spawn` parses an optional checked decimal u64 and reports the new task ID.
  `kill` terminates a non-current task; `wait` prints and consumes its status.
  Spawned programs share the console, so output can interleave with the prompt.
- Fastfetch converts ticks to seconds with hundredths and bytes to tenths of a
  MiB. Its three memory counters come from SYS_MEMINFO, not hardcoded free RAM.
- The default clean boot selects task ID 2 (shell) after nameserver ID 1.
  `demo-tasks` selects task ID 1 and preserves the earlier regression IDs.

## Userspace allocation

- `userspace/src/allocator.rs` stores a 16-byte header before every payload;
  header `size` is the total block size and `next` is used only while free.
- `malloc` aligns payloads to 16 bytes, returns null for zero/overflow/OOM,
  uses first-fit and splits when at least 32 bytes remain. If no block fits,
  it extends `brk` and creates a new block there.
- `free` inserts by address and coalesces with both neighbors. If the final
  free block ends at the current break, `trim_program_break` first shrinks
  `brk`, then unlinks that block. Thus tail memory returns to the kernel.
- Allocator state lives in each task's private ELF data. It currently assumes
  one execution thread per address space and has no double-free validation.
- The demo ELF registers `UserAllocator` as `#[global_allocator]` and imports
  `extern crate alloc`. `alloc` forwards `alloc`/`dealloc` to `malloc/free`;
  default `realloc` uses those operations plus a copy.
- Layout alignment above 16 bytes is not supported yet. It and OOM print the
  layout and exit the current EL0 task. This explicit error path avoids the
  still-unstable `#[alloc_error_handler]`; do not return null into `alloc`.

## Page-table, copyin/copyout and PAN rules

- User ELF text is AP=0b11 (read-only at EL1 and EL0), executable at EL0 and
  PXN. User rodata is read-only and UXN/PXN.
- User data, heap pages, stacks and private pages are AP=0b01 and UXN/PXN.
  TTBR1 MMIO and DRAM mappings are AP=0b00 and absent from TTBR0.
- AP=0b10 is not user read-only: with the two-range EL1/EL0 regime it means
  read-only at EL1 and no EL0 access. Use AP=0b11 for user-readable rodata.
- A page writable at EL0 (AP=0b01) is execute-never at EL1 regardless of PXN.
  Kernel execution remains in TTBR1; user text is read-only and PXN.
- `mmu::valid_user_slice()` requires EL0-read permission; the `_mut` variant
  requires AP=0b01 write permission. Every page in the range is walked through
  current TTBR0 and the 4096-byte cap remains.
- `user_memory::copy_from_user` uses `LDTRB`; `copy_to_user` uses `STTRB`.
  Kernel-side sources and destinations are ordinary high-half buffers. No
  arbitrary kernel code should dereference a raw user pointer directly.
- PAN is enabled in both the boot stub and `configure_and_enable` (SCTLR bit
  22). LDTR/STTR perform unprivileged access even with PAN set.
- `sys_receive` validates the output buffer for write access before blocking,
  then copyout checks it again after wakeup. Read-only and unmapped buffers
  return SYSCALL_ERROR without consuming a message.

## Architectural invariants (do not break!)

1. **Single-CPU.** Every `unsafe impl Sync` on an UnsafeCell wrapper
   (`TaskManager`, `IpcState`) rests on a single CPU core. Real spinlocks
   come before SMP.
2. **Critical sections via DAIF.** Global kernel state (scheduler, IPC) is
   mutated only under `save_and_disable_irq` / `restore_irq`. That is the
   only mutual-exclusion mechanism besides Mutex + Condvar.
3. **The DAIF rule for tasks.** A voluntary context switch (`sleep` /
   `yield` / `block_current_on`) inherits masked IRQs. Kernel task entry points
   must call `interrupts::enable_irq()` before waiting; idle does this.
   User tasks get unmasked PSTATE from `silver_user_bootstrap` and from the
   SPSR saved in their exception frames.
4. **IRQ handler order** (`gic::silver_handle_irq`):
   `timer::handle_irq()` → `end_interrupt()` (EOI) → `scheduler::schedule()`.
   EOI strictly before the switch.
5. **The heap is initialized after the frame allocator** and only through
   `alloc_contiguous_pages`. No `alloc` before `allocator::init()`.
6. **Context switch layout:** x19-x30 at offsets 0-95, SP_EL1 at 96,
   SP_EL0 at 104, resume PC (`TaskContext.elr`) at 112 — 120 bytes total.
   `silver_context_switch` saves/restores this block and finishes with
   `ret`. On save, x30 is copied into the resume-PC slot; on restore, the
   resume-PC slot wins over the saved x30. Fresh tasks put their entry or
   `silver_user_bootstrap` there.
7. **Task states:** `Ready / Running / Sleeping(deadline_ticks) /
   Blocked / BlockedUntil(deadline_ticks) / Dead`. The scheduler gives the CPU
   only to Ready tasks; sleeping and blocked tasks are skipped. One timer scan
   wakes expired sleepers and timed waiters; `find_next_task` prefers non-idle
   tasks.
8. **Both trampolines keep ELR/SPSR on the stack.** Each builds a 272-byte
   frame: x0-x30, ELR at offset 248, SPSR at offset 256. The epilogue
   restores ELR/SPSR from the frame before `eret`. They mask IRQs on entry
   and do not need to re-arm ELR/SPSR for scheduling: the ret-based switch
   unwinds back to the epilogue with the original SP. CPU-global ELR/SPSR
   may be clobbered by other tasks in between; the frame copy is authoritative.
9. **SVC register preservation starts before ESR decoding.** x9/x10 are user
   registers, not free scratch. The sync entry saves/restores them around its
   dispatch check; the full 272-byte frame is built afterwards.
10. **Nameserver holds the well-known ID NAMESERVER_ID = 1**, fixed by spawn
    order in `scheduler::init` (first after idle). Protocol: text over mailbox
    messages, "R <name>" → "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING",
    and "U <name>" → "OK"/"MISSING"/"NOT OWNER". Kernel RPC uses
    `receive_from(NAMESERVER_ID)`, so unrelated queued messages are preserved.
11. **Cross-crate syscall structures have a stable layout.** `Message` is
    `#[repr(C)]` in kernel and userspace. Apply the same rule to future
    structs passed by pointer across the SVC boundary.
12. **TLB/cache ritual for any future table edit.** After modifying tables,
    clean lines to the point of coherency (`dc cvac` + `dsb`), invalidate
    stale TLB entries (`tlbi` + `dsb` + `isb`). Template:
    `mmu::flush_after_table_change`. Address-space switches load TTBR0_EL1
    and issue `tlbi vmalle1; dsb ish; isb` before the context switch.
13. **UART FIFO waits preserve DAIF.** `pl011::write_byte()` may unmask IRQs
    while waiting for TXFF, but restores the caller's saved DAIF before
    returning. Never replace this with an indefinitely masked spin. Taking
    a timer IRQ mid-print is intentional and safe with the frame-based
    trampolines.
14. **Fault diagnostics must not create a second fault.** Validate every
    userspace stack word before printing it; print `<unmapped>` at the first
    invalid word.

## Why IPC uses Mutex + Condvar

IPC state lives behind `Mutex<IpcState>`. Every mailbox owns boxed `not_empty`
and `not_full` Condvars; boxing keeps their addresses stable when the mailbox
Vec reallocates. `Condvar::wait`/`wait_until` releases the mutex before blocking.
Send wakes one receiver, receive wakes one sender, and mailbox destruction wakes
all senders. Check the condition and wait under the lock, notify under the lock,
and re-check after waking. Timed waits share the scheduler's absolute tick
deadlines and remove themselves from the queue on every return path.

IRQ masking guards the tiniest scheduler-critical sections; Mutex + Condvar
guard everything that can wait on a condition. With a bounded queue, a task that
selectively waits for one sender can leave its mailbox full and block the very
sender it needs. Likewise, nameserver replies obey backpressure and can block if
a client's mailbox is full; demos must not issue RPC to an already-full mailbox.

## Debugging aids

`scheduler.rs` keeps a 16-entry ring of recent context switches
(`from`, `to`, next SP, next resume PC). `silver_handle_task_fault` dumps it
before killing the task. It costs no output during normal operation; keep it
until task lifecycle and isolation are more battle-tested. Fault stack dumps
stop at the first unmapped word.

QEMU exception tracing is invaluable for resume/fault bugs:

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel \
  -d int -D qemu-int.log
```

## Known tech debt

- The loader supports only static `ET_EXEC`, a contiguous 2 MB image window
  and page-uniform permissions; there is no PIE, dynamic linking, demand
  loading or sparse multi-window image layout.
- Programs are selected from ELFs embedded at kernel build time. There is no
  external program storage, console ownership/job control or credential model yet.
- Mutex and condvar wait queues still have no growth limits. Mailboxes are
  bounded at 16 messages, but nameserver/control replies also obey backpressure;
  a full client mailbox can therefore block the nameserver. Selective receive
  on a full mailbox can likewise deadlock with its expected sender.
- The user heap is limited to one 2 MB L3 window. The allocator supports only
  alignments up to 16 bytes and has no explicit C-style `realloc`/`calloc`,
  thread safety or double-free detection. There is no general `mmap`/`munmap`
  or growable stack; the user stack remains fixed at 8 KB and the private
  window at one page.
- UART lines from different tasks can interleave; an IRQ may even schedule
  another task during a FIFO wait. Shell RX is polled rather than interrupt
  driven, has no escape-sequence parser and currently has one intended reader.
- `#[alloc_error_handler]` is unstable on stable Rust. The kernel converts a
  null from its `GlobalAlloc` into a panic; the demo userspace wrapper instead
  prints the layout and exits only the current EL0 task.

## Next steps (by priority)

1. **Console ownership and job control:** spawned programs currently share UART
   output with the shell; add foreground/background ownership before RX IRQs.
2. **Grow VFS semantics:** append mode, file-size quotas and overwrite/rename
   options if needed. Persistent storage still needs a block driver.
3. **Generalize per-task VM:** sparse multi-window ELF images, `mmap`/`munmap`,
   growable stacks and demand paging later. Add over-aligned user allocations
   and explicit `realloc`/`calloc` when their use cases appear.
4. **VirtIO-console / virtio-block** — first MMIO device driver, moving
   UART output behind a driver interface. UART RX interrupts can remove shell
   polling when console ownership is defined.
5. **Enable caches** (SCTLR.C/I): once enabled, the table-clean ritual in
   `mmu::flush_after_table_change` becomes mandatory, not ceremonial.

Nameserver watch notifications are explicitly deferred. Later IPC control-plane
work should also address nonblocking/priority replies, send timeouts/cancellation
and the full-mailbox selective-receive limitation recorded above.

## Workflow for a new feature

1. Code → `cargo fmt --all` and `cargo fmt --manifest-path userspace/Cargo.toml`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. Also check `--release`: optimized userspace exposes register-preservation
   bugs that debug builds can hide. For regressions, separately build with
   `--features no_std,demo-tasks`; this replaces the interactive boot.
5. For the default profile, verify `ls`, `echo 'hello' > test.txt`,
   `cat test.txt`, `mkdir docs`, quoted echo, `cd /bin`, `pwd`, relative paths,
   `cd ..`, `ps`, and `spawn`/`kill`/`wait`. Also check fastfetch and the logo. For
   demo-tasks, run 5–30 seconds,
   repeat timing-sensitive runs and expect exactly two intentional faults from
   tasks 15 and 16, plus
   `COPYOUT READONLY REJECT=PASS`, `COPYOUT UNMAPPED REJECT=PASS`,
   `LIFECYCLE EXIT/UNREGISTER=PASS`, `LIFECYCLE MUTEX CLEANUP=PASS`,
   `HEAP GROW/SHRINK=PASS`, `MALLOC FREE/REUSE=PASS`,
   `MALLOC BRK TRIM=PASS`, `GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS`,
   `GLOBAL ALLOC TRIM=PASS`, `IPC TIMEOUT=PASS`,
   `IPC BP SENDER RESUMED=PASS`, `IPC BP BACKPRESSURE=PASS` and
   `IPC EXIT WAKE=PASS`.
6. Update the README checklist and this file.
