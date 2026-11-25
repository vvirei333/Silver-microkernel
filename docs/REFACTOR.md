# Modularisation plan

The kernel and the console userspace grew by adding features to the files that
were already open. The scheduler (8 755 lines) and the shell (2 172) have since
been split; `src/scheduler/mod.rs` (4 398) is the largest file left. Nothing in this
plan changes behaviour. Every step is a pure move with the same verification,
and each step is its own commit so a regression can be bisected to a move.

## Rules for a refactor step

1. **No behaviour change in the same commit.** A move and a fix never travel
   together; if a move reveals a bug, the bug gets the next commit.
2. **Same verification as a release.** `cargo test --lib`, `cargo build` for
   `no_std` and `no_std,linux-abi`, `scripts/release_smoke.py` (8/8), and the
   `smp-selftest` boot on `-smp 4` (16/16) when the scheduler is touched.
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
| `silvershell.rs` → `shell/` | The 2 172-line shell binary became a 183-line bin (`_start`, input loop, `HowlerLink`, panic handler) plus `shell/mod.rs` (48 lines: shared constants and `prompt`/`echo`/`clear_screen`) and one module per command family: `commands.rs` (430: dispatcher, `fastfetch`, watchpoint), `files.rs` (243: SFS write path, directory operations, `du`), `net.rs` (384: `ping`/`ip`/`dns`/`wget`/`wolfser`), `proc.rs` (638: task views, spawn/kill/wait, ELF loaders, reaping) and `bench.rs` (335: cycle benchmarks and `memtest`). Every moved item is verbatim (re-extracted all 123 top-level items and matched them against the new files); only visibility was added — `pub(super)` inside the family modules, `pub` for the dispatcher/`fastfetch`/driver lookup and for the imports the bin needs. The dispatcher glob-imports the families because it is the one place that names them all. Verified: 249 tests, userspace builds in all four feature combinations (default, `linux-abi`, `tablet`, both), kernel `no_std`, `+linux-abi` and `+smp-selftest`, smoke 8/8 (its `ls`, `cat`, `echo`, `linuxrun`, `bench` and `health` checks all go through the split binary), `smp-selftest` 10/10 PASS on `-smp 4`, and a tree boot that still prints the `DRIVER ...=DT`/`SIMPLEFB`/`HEAP` evidence and reaches `howl>`. |
| `linux_abi.rs` → `linux_abi/` | The 3 542-line syscall personality became `linux_abi/mod.rs` (142 lines: shared errno values and ABI flags, the trace/fail rings, `errno`, `finish`, `user_u64`, `iovec_at`, re-exports) plus `dispatch.rs` (461: the 133 `SYS_*` numbers and the `dispatch`/`dispatch_body`/`dispatch_inner` match), `fs.rs` (1 160: paths, descriptors, `openat`/`statx`/`readlinkat`, read/write/ioctl, mount), `process.rs` (669: fork/exec, signals, time, rlimit/uname), `memory.rs` (335: brk/mmap/mprotect/futex) and `ipc.rs` (801: pipes, sockets, epoll, timerfd, poll). All 291 top-level items were re-extracted from the original and matched verbatim, with no lost source lines. Visibility was the only change: `pub(super)` on the 90 family functions plus `INOTIFY_WD` and `MsgHdr`, `pub` kept for the six existing entry points, and `mod.rs` re-exports `dispatch`, `SYS_RT_SIGRETURN`, `map_absolute` and `resolve`. The number table and the rings stayed with dispatch (the only reader); the errno/flag table stayed shared; futex state followed memory, `MsgHdr` followed ipc and the inotify id followed fs. `process` imports three helpers from `fs` (path resolution and user strings), everything else goes through `mod.rs`. Verified: 187 warnings before and after (the same 8 pre-existing `linux_abi` ones), 249 tests, seven feature builds (`no_std`, `linux-abi`, `linux-abi-trace`, `+smp-selftest`, `fog`, `tablet`, `gdb-stub`), smoke 8/8 (the Linux ELF exit-42 path), `smp-selftest` 10/10 PASS on `-smp 4`, tree boot with `DRIVER ...=DT`/`SIMPLEFB`/`HEAP` reaching `howl>`, and `disk.img` byte-identical. |
| `mmu.rs` → `mmu/` | The 3 465-line memory-management unit became `mmu/mod.rs` (317 lines: module docs, shared constants and statics, the `AsidPool`/`Table` types, the `#[no_mangle] TABLES` registry, re-exports) plus `table.rs` (326: page-table registry and ownership, ASID allocation, descriptor/entry access, `zero_page`), `kernel.rs` (401: TTBR1 map, boot control registers, cache maintenance, address-space switch), `user.rs` (2 074: per-task spaces, mappings/protection/COW, Linux image and DMA windows) and `selftest.rs` (389: the six boot probes). All 188 top-level items were re-extracted and matched against the original (line multiset: 0 missing); visibility was the only change — bare functions became `pub(super)`, while `pub`/`pub(crate)` entry points and constants are re-exported from `mod.rs` with `#[allow(unused_imports)]` because different feature sets use different subsets (the functions keep their pre-existing dead-code warnings). One trailing doc comment that documented `zero_page` across a blank line moved with it; otherwise no source line changed. Verified: 187 warnings before and after (the same 4 pre-existing `mmu` ones), 249 tests, eight feature builds (`no_std`, `linux-abi`, `linux-abi-trace`, `+smp-selftest`, `smp`, `fog`, `tablet`, `gdb-stub`), smoke 8/8, `smp-selftest` 10/10 PASS on `-smp 4`, tree boot with `DRIVER ...=DT`/`SIMPLEFB`/`HEAP` reaching `howl>`, and `disk.img` byte-identical. |
| `scheduler/wait.rs` | Task death and waiting: `exit_current*`, `kill_task`, `recover_console`, `wait_for_task*`, `wait_status_for_pid`, `consume_zombie`, `wait_inner`, `take_zombie`, `reparent_children`, `reaper_pid`, `exit_group`, `WaitEvent` — 452 moved lines. `mod.rs` 5 975 → 5 510. The public entry points stay `pub` and are re-exported (`pub use wait::*`), so `scheduler::exit_current()` and friends are unchanged outside; parent-only helpers (`take_zombie`, `recover_console`, `has_task_capacity`) are `pub(super)`. Two mmap quota constants that sat in the middle of the block went to `task.rs`, where every use lives. Verified: 240 tests, smoke 8/8 (includes the Linux ELF exit-42 wait path), `smp-selftest` 10/10 PASS on `-smp 4`, health 0 zombie. |

## Next, in order

All planned splits are done. The largest files left are `scheduler/mod.rs`
(4 398), `linux_fd.rs` (3 096), `drm.rs` (2 781) and `fdt.rs` (2 526); any
future step starts from a measured seam, not from a line count.

## Repo hygiene that goes with it

- Documents describe current behaviour only; superseded text is moved to
  `docs/archive/` instead of being left in place with a warning.
- `docs/README.md` is the index; `README.md` stays the landing page.
- Release artifacts are produced by `scripts/make_release.sh` and never
  committed (`dist/` is ignored).
- The tracked `disk.img` must stay byte-identical across boots: seeding is
  idempotent (`src/seed.rs`), and a boot that mutates it is a bug.
