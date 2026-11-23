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
| `scheduler/task.rs` | `Task`, `TaskState`, `TaskContext`, `TaskInfo`, `Personality`, `MmapRegion`, `SharedMapping` and the whole `impl Task` block (`brk`/`mmap`/region bookkeeping, ~1 730 lines) — 1 966 moved lines. `mod.rs` 7 939 → 5 975. Fields the parent still reads are `pub(super)`; nothing left the module. Verified: 229 tests, smoke 8/8, `smp-selftest` 9/9, `bench` unchanged (yield 6.0 µs, SFS open 21.6 µs). |
| `scheduler/spawn.rs` | Task creation: `spawn_kernel`, `spawn_idle`, `spawn_user*`, `spawn_elf*`, `spawn_linux_elf_with_args`, `spawn_thread`, `spawn_elf_task`, `spawn_inner`, `initial_self_rights` — 571 moved lines. `mod.rs` 5 510 → 4 945. Boot-only helpers became `pub(super)`; the entry points other subsystems call are `pub` and re-exported. `current_task_pid` stayed (fork/exec uses it too). Verified: 240 tests, smoke 8/8, `smp-selftest` 10/10 on `-smp 4`, health 0 zombie. |
| `scheduler/wait.rs` | Task death and waiting: `exit_current*`, `kill_task`, `recover_console`, `wait_for_task*`, `wait_status_for_pid`, `consume_zombie`, `wait_inner`, `take_zombie`, `reparent_children`, `reaper_pid`, `exit_group`, `WaitEvent` — 452 moved lines. `mod.rs` 5 975 → 5 510. The public entry points stay `pub` and are re-exported (`pub use wait::*`), so `scheduler::exit_current()` and friends are unchanged outside; parent-only helpers (`take_zombie`, `recover_console`, `has_task_capacity`) are `pub(super)`. Two mmap quota constants that sat in the middle of the block went to `task.rs`, where every use lives. Verified: 240 tests, smoke 8/8 (includes the Linux ELF exit-42 wait path), `smp-selftest` 10/10 PASS on `-smp 4`, health 0 zombie. |

## Next, in order

| # | Step | Target | Why this order |
|---|---|---|---|
| 1 | `scheduler/switch.rs` | `schedule`, `schedule_released`, `find_next_task`, ready queues, context-switch asm | The core gets the smallest, easiest-to-review file once the rest is out |
| 2 | `main.rs` → `boot/` | boot milestones into `boot/console.rs` / `boot/mem.rs` / `boot/selftest.rs` | Boot order is linear; split by milestone group, not by line count |
| 3 | `silvershell.rs` → `shell/` | `shell/commands.rs`, `shell/files.rs`, `shell/net.rs`, `shell/proc.rs`, `shell/bench.rs`, keeping `_start`/loop in the bin | The shell is the console product's front door; splitting it makes command work reviewable |
| 4 | `linux_abi.rs` → `linux_abi/` | `dispatch.rs`, `process.rs`, `memory.rs`, `fs.rs`, `ipc.rs` | Largest remaining file after the scheduler; the ABI groups are already visible in the dispatch table |

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
