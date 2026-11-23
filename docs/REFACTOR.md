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
3. **Do not widen visibility to make a move possible.** A submodule can read
   its parent's private items, so probe/test code moves as a *child* module
   instead of forcing `pub(crate)` on internals. If a move needs a new `pub`,
   the seam is probably in the wrong place.
4. **One seam per commit**, and the file keeps compiling on its own: no
   half-moved modules on `main`.

## Done

| Step | Result |
|---|---|
| `src/scheduler.rs` → module directory | `scheduler/mod.rs` (7 939 lines) + `scheduler/selftest.rs` (849 lines). Moved the 11 `run_*` boot probes, their probe task entries and the churn stress state; re-exported so callers are untouched. Verified: 229 tests, smoke 8/8, `smp-selftest` 9/9. |

## Next, in order

| # | Step | Target | Why this order |
|---|---|---|---|
| 1 | `scheduler/task.rs` | `Task`, `TaskContext`, `TaskState`, `TaskInfo`, `MmapRegion`, `SharedMapping`, `Personality` (~600 lines) | The data types are the stable part; moving them first shrinks every later diff |
| 2 | `scheduler/wait.rs` | `exit_current*`, `exit_group`, `kill_task`, `wait_*`, `consume_zombie`, zombie reaping, reparenting (~700 lines) | Second-most self-contained concern; it owns the death/wake state machine |
| 3 | `scheduler/spawn.rs` | `spawn_inner`, `spawn_user*`, `spawn_elf*`, console-handoff variants, `initial_self_rights` (~800 lines) | Depends on 1 and 2 |
| 4 | `scheduler/switch.rs` | `schedule`, `schedule_released`, `find_next_task`, ready queues, context-switch asm | The core gets the smallest, easiest-to-review file once the rest is out |
| 5 | `main.rs` → `boot/` | boot milestones into `boot/console.rs` / `boot/mem.rs` / `boot/selftest.rs` | Boot order is linear; split by milestone group, not by line count |
| 6 | `silvershell.rs` → `shell/` | `shell/commands.rs`, `shell/files.rs`, `shell/net.rs`, `shell/proc.rs`, `shell/bench.rs`, keeping `_start`/loop in the bin | The shell is the console product's front door; splitting it makes command work reviewable |
| 7 | `linux_abi.rs` → `linux_abi/` | `dispatch.rs`, `process.rs`, `memory.rs`, `fs.rs`, `ipc.rs` | Largest remaining file after the scheduler; the ABI groups are already visible in the dispatch table |

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
