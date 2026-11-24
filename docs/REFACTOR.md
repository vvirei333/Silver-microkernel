# Modularisation plan

The kernel and the console userspace grew by adding features to the files that
were already open: `src/scheduler.rs` reached 8 755 lines, `src/linux_abi.rs`
3 542, `src/mmu.rs` 3 465, `src/main.rs` 1 875 and
`userspace/src/bin/silvershell.rs` 2 172. Nothing in this plan changes
behaviour. Every step is a pure move with the same verification, and each step
is its own commit so a regression can be bisected to a move.

## Rules for a refactor step

1. **No behaviour change in the same commit.** A move and a fix never travel
   together; if a move reveals a bug, the bug gets the next commit.
2. **Same verification as a release.** `cargo test --lib`, `cargo build` for
   `no_std` and `no_std,linux-abi`, `scripts/release_smoke.py` (8/8), and the
   `smp-selftest` boot on `-smp 4` (9/9) when the scheduler is touched.
3. **Widen visibility only as far as the move forces, and never past the
   module.** Code that *reads* parent state moves as a child module and needs no
   widening at all (a descendant may read its parent's private items). Moving
   *data* out is the one case that needs it: fields the parent still reads
   become `pub(super)` — visible inside `scheduler`, invisible to the rest of
   the crate. `pub(crate)` in a refactor commit is a red flag.
4. **One seam per commit**, and the file keeps compiling on its own: no
   half-moved modules on `main`.

## Done

| Step | Result |
|---|---|
| `src/scheduler.rs` → module directory | `scheduler/mod.rs` + `scheduler/selftest.rs` (849 lines). Moved the 11 `run_*` boot probes, their probe task entries and the churn stress state; re-exported so callers are untouched. |
| `main.rs` → `boot/asm.rs` | The 1 243 lines of `global_asm!` (entry stub, exception vectors, assembler macros) moved out of `main.rs` into `boot/asm.rs`; `main.rs` 1 912 → 669 lines. The `sym`/`const` operands were already crate-absolute, so the generated image is unchanged: same boot log, same self-tests. |
| `main.rs` → `boot/` milestones | `kernel_main`'s remaining milestone groups became `boot/console.rs` (105 lines), `boot/mem.rs` (116), `boot/selftest.rs` (130), `boot/interrupts.rs` (64) and `boot/start.rs` (40); `main.rs` 669 → 303, so `kernel_main` now reads as the boot order it implements: entry, console, memory, filesystems, self-tests, interrupts, scheduler. The heap probe allocation is returned to `kernel_main` so it still outlives the self-tests that exercise the allocator, and the deep-test switch moved with the tests it guards. Modules are `pub(super)`: the crate root is the only caller. Verified: 249 tests, nine feature builds (including `fog-timer-test`, `fog-auto-reset`, `gdb-stub`, `tablet`), smoke 8/8, `smp-selftest` 10/10 PASS on `-smp 4`, and a tree boot printing the same `SIMPLEFB`/`HEAP`/`DRIVER ...=DT` evidence. |
| `scheduler/switch.rs` | `start`, `schedule_released`, `schedule`, plus the ready-queue engine (`ready_for_queue`, `enqueue_ready`, `dequeue_ready`, `pop_best`, `rebuild_ready_queues`, `cpu_is_idle`, `push_migrate*`, `find_next_task`) — 553 moved lines. `mod.rs` 4 945 → 4 398. Queue helpers are `pub(super)`; `spawn` imports `enqueue_ready` from the sibling module explicitly. Verified: 244 tests, smoke 8/8, `smp-selftest` 10/10 on `-smp 4`. |
| `scheduler/task.rs` | `Task`, `TaskState`, `TaskContext`, `TaskInfo`, `Personality`, `MmapRegion`, `SharedMapping` and the whole `impl Task` block (`brk`/`mmap`/region bookkeeping, ~1 730 lines) — 1 966 moved lines. `mod.rs` 7 939 → 5 975. Fields the parent still reads are `pub(super)`; nothing left the module. Verified: 229 tests, smoke 8/8, `smp-selftest` 9/9, `bench` unchanged (yield 6.0 µs, SFS open 21.6 µs). |
| `scheduler/spawn.rs` | Task creation: `spawn_kernel`, `spawn_idle`, `spawn_user*`, `spawn_elf*`, `spawn_linux_elf_with_args`, `spawn_thread`, `spawn_elf_task`, `spawn_inner`, `initial_self_rights` — 571 moved lines. `mod.rs` 5 510 → 4 945. Boot-only helpers became `pub(super)`; the entry points other subsystems call are `pub` and re-exported. `current_task_pid` stayed (fork/exec uses it too). Verified: 240 tests, smoke 8/8, `smp-selftest` 10/10 on `-smp 4`, health 0 zombie. |
| `scheduler/wait.rs` | Task death and waiting: `exit_current*`, `kill_task`, `recover_console`, `wait_for_task*`, `wait_status_for_pid`, `consume_zombie`, `wait_inner`, `take_zombie`, `reparent_children`, `reaper_pid`, `exit_group`, `WaitEvent` — 452 moved lines. `mod.rs` 5 975 → 5 510. The public entry points stay `pub` and are re-exported (`pub use wait::*`), so `scheduler::exit_current()` and friends are unchanged outside; parent-only helpers (`take_zombie`, `recover_console`, `has_task_capacity`) are `pub(super)`. Two mmap quota constants that sat in the middle of the block went to `task.rs`, where every use lives. Verified: 240 tests, smoke 8/8 (includes the Linux ELF exit-42 wait path), `smp-selftest` 10/10 PASS on `-smp 4`, health 0 zombie. |

## Next, in order

| # | Step | Target | Why this order |
|---|---|---|---|
| 1 | `silvershell.rs` → `shell/` | `shell/commands.rs`, `shell/files.rs`, `shell/net.rs`, `shell/proc.rs`, `shell/bench.rs`, keeping `_start`/loop in the bin | The shell is the console product's front door; splitting it makes command work reviewable |
| 2 | `linux_abi.rs` → `linux_abi/` | `dispatch.rs`, `process.rs`, `memory.rs`, `fs.rs`, `ipc.rs` | Largest remaining file after the scheduler; the ABI groups are already visible in the dispatch table |

Sizes are estimates from the current line counts; each step is expected to be
200–900 moved lines with no logic edits.

## Repo hygiene that goes with it

- Documents describe current behaviour only; superseded text is moved to
  `docs/archive/` instead of being left in place with a warning.
- `docs/README.md` is the index; `README.md` stays the landing page.
- Release artifacts are produced by `scripts/make_release.sh` and never
  committed (`dist/` is ignored).
- The tracked `disk.img` must stay byte-identical across boots: seeding is
  idempotent (`src/seed.rs`), and a boot that mutates it is a bug.
