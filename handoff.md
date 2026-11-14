# Handoff: silver_microkernel

Continuation document: current state, architectural invariants, known tech
debt and recommended next steps. Update after every significant kernel change.

## CURRENT STATUS (2026-11-09)

Active vector (see [`docs/VECTOR.md`](docs/VECTOR.md)):

1. console OS on QEMU/desktop is the **base OS** (kernel + SFS + network +
   Howl + toolbox). Drivers, networking and basic operations stay native.
2. applications (browser included) come **from Linux** through the optional
   `linux-abi` compatibility layer, not from new native userspace;
   Wolfser is a temporary browser (stopgap) and is frozen except for bug fixes.
3. **Linux ABI + Phosh** as the active long-term goal (owner decision:
   months, not years), via a kernel personality layer and software-rendered
   headless Wayland exported to the host window.

The graphics subsystem was removed on 2026-11-08 (compositor, framebuffer
backends, kernel surface objects, input driver, home screen, `fog-paint`/
`fog-ui`/`fog-diag`/`fog-display`). The Legion Y700 tablet is forgotten; the
Redmi port is parked. The reference checklist is
[`docs/REFERENCE.md`](docs/REFERENCE.md).

- **Host tests:** 221/221 (`cargo test --lib`).
- **Builds:** `no_std` (product), `no_std,linux-abi` (Linux module on),
  `no_std,linux-abi-trace`, `no_std,fog`, `no_std,fog,fog-reloc`,
  `no_std,fog,fog-auto-reset`, `no_std,fog,fog-hw-selftests`,
  `no_std,tablet` (parked), `no_std,smp-selftest`, `no_std,demo-tasks`.
- **Console OS (QEMU serial):** boots to `howl ready` with no graphics tasks;
  `help`, `uname`, `ls /`, `health` all answer. `health` reports no DMA leaks.
  The kernel/userspace version is **0.1.1**. `fastfetch` prints an aligned
  two-column summary — OS/kernel, codename, arch, platform, shell, FS, uptime,
  memory with a percentage, processes, open files, SFS usage and DMA state —
  followed by the 16-colour bar, and `uname` reports `Silver Kernel v0.1.1`.
  A real dynamic glibc binary (`ld.so` + `libc.so.6`) loads and prints
  `dynamic hello` through the Linux ABI.
- **Network:** `net_server` runs DHCP (with the static fallback), DNS, TCP and
  HTTP. `ip` prints the lease, `ping 10.0.2.2` is OK, `dns example.com`
  resolves, `wget http://example.com/ /tmp/e.html` stores a 577-byte page, and
  `https://` fails fast with a clear message. A client data window carries
  URLs/names in and HTTP bodies out (the 32-byte mailbox is only doorbells).
- **smoltcp patch:** `third_party/smoltcp` vendors 0.14.0 with two guarded
  returns in the neighbour path; without them DHCP boots intermittently hit
  the cache's unicast assertion. See `third_party/smoltcp/PATCH.md`.
- **TLS:** `net_server` now runs a TLS 1.3 client (`third_party/embedded-tls`
  with four local verifier fixes) and fetches HTTPS itself. The SAN DNS-name
  limit grew from 3 to 32: the Wikimedia wildcard carries 40 names and
  `thumb.wikimedia.org` only matches `*.wikimedia.org` at index 17, so image
  hosts failed while article hosts matched the CN. Six roots cover the
  current search engine, Wikipedia, example.com, Microsoft and Sectigo; RSA,
  ECDSA P-256/P-384 and Ed25519 are enabled. `wget https://...` works, and the
  browser uses guest TLS first. The viewer's Qt proxy remains the fallback for
  sites outside the root store or TLS 1.2-only servers. Certificate time is a
  fixed 2026-11-08 until an RTC exists; the RNG is a seeded xorshift, not a
  hardware entropy source. The user stack grew from 8 KiB to 256 KiB because
  the handshake, the TLS state machine and litehtml's layout recursion need
  the headroom, and the client body window grew to 1 MiB so real pages are
  not truncated.
- **Wolfser:** `wolfser` opens with an empty search bar (or `wolfser <query|url>`
  navigates immediately). Input resolves to an `https://` URL (bare hosts) or a
  DuckDuckGo HTML search. `http://` and `https://` both go through
  `net_server`/smoltcp; the Qt viewer proxy is only the fallback and the image
  decoder. Redirects
  (3xx and meta refresh) are resolved by the guest and may switch backends.
  The primary renderer is **litehtml** (New BSD, bundled gumbo HTML5 parser),
  built for `aarch64-none-elf` with the xPack newlib/libstdc++ toolchain and
  linked only into `wolfser`. `userspace/engine/wolf_container.cpp` implements
  the `document_container` over the RGBA surface and the 5x7 font, so litehtml
  supplies real block/inline layout, margins, colours, lists, borders and
  hit-testing. For pages with `<div id="mw-content-text">`, `<main>` or
  `<article>`, Wolfser extracts that region, drops scripts/styles/tables,
  hidden elements and `div`/`span` wrappers, keeps structural tags plus
  `href`, and renders a compact reader view with a built-in stylesheet. Other
  pages render as-is with inline CSS and external stylesheets up to 64 KiB.
  The reader slice is capped at 48 KiB. Six tabs keep URL, title, scroll,
  history and a full cache of the extracted page (text, styles, links);
  switching is instant and does not refetch. The old text/inline-image
  renderer remains as a fallback when litehtml cannot start. `Ctrl+T`/`Ctrl+W`/`Ctrl+Tab`, the tab strip, `+` and the close `x`
  manage them. Hovering a link publishes its target in the footer and sets
  `FIELD_CURSOR` so the viewer shows a hand; middle click/Ctrl+click opens the
  target in a new tab. The input ring now advances its consumer index before
  handling a key (a blocking navigation can no longer replay it), the viewer
  drops Qt auto-repeat and identical events within 20 ms, and the host copies
  each completed frame into a QImage before painting. Loading keeps the old
  document or start page visible with a footer spinner, so searches do not
  flash a white screen. The litehtml path now fetches `<img>` sources through
  the guest net_server and decodes them in-guest with `stb_image`
  (PNG/JPEG/GIF): up to four per page, 512 KiB encoded and 300k pixels each,
  cached as RGBA and drawn scaled. Images no longer block the first paint:
  the document is created in collect mode, the browser fetches one image at a
  time while the loop stays responsive, and rebuilds the document from the
  cache when the queue drains (scroll offset preserved). The cache survives
  document rebuilds, and tab switches only reuse it (no network fetch), so
  returning to a page is instant even if it has images. It is capped at eight
  images / 3M pixels and evicts the oldest site. The old text renderer keeps
  its viewer-side image path as a fallback.
  The first line of every page used to lose its spaces because the wrap buffer
  had not flushed yet; a `line_len` check fixed it. Bookmarks are
  persisted in `/sfs/wolfser.bookmarks` using a path-scoped write window
  (no global CAP_FS_WRITE). Six tabs fit the strip, and `Ctrl++`/`Ctrl+-`/
  `Ctrl+0` resize/zoom the host window (input coordinates are scaled back).
  A small DNS cache in net_server keeps repeated navigation from re-resolving
  every host. Pages with a text input expose it through `Ctrl+G`: the footer
  becomes an editable field, Enter submits the form's action with GET or POST
  (hidden inputs included), and Esc cancels. POST uses the same guest TLS/HTTP
  stack; the proxy fallback is disabled for POST. A session cookie jar stores
  `Set-Cookie` per host and sends `Cookie` on later requests; this made
  DuckDuckGo's POST search work. `tools/form-test-server.py` serves a local
  GET/POST form (and sets a cookie) for a deterministic QEMU check. `Ctrl+F` searches the text,
  highlights all hits yellow and the current hit orange, shows `n/total` in the
  footer and Enter advances (wrapping). Partially scrolled lines are skipped
  rather than clamped to the body top, which used to overlap the next line.
  A new tab or `Alt+Home`/the `H` button opens a start page with three quick
  tiles and the pages saved by `Ctrl+D`; the list is reloaded from SFS at
  startup. `tools/window-viewer` (Qt6 Widgets + Network) maps the file,
  repaints on `frame_seq` and sends keys, wheel and clicks through the ring.
  The sequence is a seqlock (odd while the guest paints); the host skips odd
  values and re-checks after the copy, so a repaint cannot tear into a white
  flash. The input ring stores positions, not counters: both host and guest
  wrap at 64. An unbounded `head` used to pin the guest in the `tail != head`
  loop after 64 events (constant repaint) while the host's `head - tail`
  underflowed and dropped every later event.
  Verified headlessly: DDG results with links, clicking the first result through
  a DDG meta-refresh redirect to the Wikipedia reader view, Alt+Left back to
  the results, typed `http://github.com/` redirecting to HTTPS, scrolling and
  reload. `SYS_MAP_PHYS_NC` maps the surface as Normal Non-Cacheable and the
  guest fences before publishing `frame_seq`.
- **Browser-sized user layout:** the linked wolfser image is ~6 MiB, so
  `elf::USER_IMAGE_SIZE` is 8 MiB and `mmu::create_task_space` maps the image
  across multiple L3 tables (extra tables are registered with the space root
  and freed by the existing teardown). The sparse heap starts at
  `USER_IMAGE_VA + 8 MiB`, anonymous mmap at `+10 MiB`, and the user stack is
  64 pages (256 KiB). The per-task mmap quota is 24 MiB; litehtml's newlib heap
  is one 20 MiB mapping, separate from the Rust `SYS_BRK` arena.
- **Linux ABI personality, slice 1 (2026-11-09).** The whole track is the
  optional `linux-abi` Cargo feature. With it off, the kernel and the console
  userspace link **no** Linux loader, syscall translation or `linuxrun`
  command (verified with `strings`/`nm` on the `no_std` binary); the rest of
  the OS has no dependency on it. `build_and_qemu.sh` enables it by default
  and `--no-linux` is the removal switch; `build.rs` forwards the kernel
  feature to the userspace crate so the shell is built without the command.
  `linuxrun <path>`
  (`SYS_SPAWN_LINUX_PATH = 98`) spawns a dynamically loaded ELF with the
  AArch64 Linux syscall ABI: `x8` is a Linux number and `src/linux_abi.rs`
  translates it onto kernel services. The personality is fixed at spawn
  (`scheduler::Personality`), so a Linux task can never fall into the Silver
  table and a native task cannot accidentally use Linux numbers. Implemented:
  `write` (1/2), `read` (fd 0 reports EOF), `exit`/`exit_group`,
  `set_tid_address`, `set_robust_list`, `clock_gettime`, `rt_sigprocmask`,
  `uname`, `getpid`/`getppid`/`gettid`, `brk`, `mmap` (hint ignored, `MAP_FIXED`
  refused), `munmap`, `mprotect` (a no-op at the time; real in slice 13),
  `prlimit64`, `getrandom`; `rseq` and
  unknown numbers return `-ENOSYS`. Errors use `-errno`, never the Silver
  sentinels. The initial stack is the real Linux layout
  (`argc/argv/envp/auxv`) built by `build_linux_stack`, with `AT_PAGESZ`,
  `AT_ENTRY`, `AT_PHDR` when the program headers sit inside a `PT_LOAD`,
  `AT_PHENT`/`AT_PHNUM`, identity entries and `AT_RANDOM` (16 bytes below `sp`).
  `scripts/sfs_inject.py` writes a file into `disk.img` offline (direct,
  indirect and double-indirect blocks), so a foreign-toolchain binary does not
  have to be embedded in the kernel. `tools/linux-test.sh` builds two
  freestanding raw-syscall programs and installs them; QEMU prints `hello from
  linux abi` (exit 42) and `RW data+bss ok` (exit 7).
  **Slice 2 (arbitrary `p_vaddr`):** `src/linux_elf.rs` honours each
  `PT_LOAD`'s own virtual address, so a normal `0x40_0000` binary loads. Every
  segment gets its own physical run, gaps are allowed, text is RX, data is RW
  and BSS is zeroed. `mmu::install_linux_run` allocates the per-2 MiB L3
  tables (and a per-1 GiB L2 for the low window) on demand and registers them
  with the space root, so `retire_space` frees them; `install_linux_image`
  maps every run after `create_task_space`. Segment page ranges may not
  overlap, W^X is rejected, and `ET_DYN`/PIE is not relocated yet. `linuxrun`
  is now foreground: `spawn_elf_task(..., foreground = true)` assigns the
  console before `enqueue_ready`, because on `-smp 2` an AP could otherwise
  run and even exit the child between the shell's `spawn` and `console_set`
  (observed as "could not assign the foreground console" plus lost output).
  Remaining gap for a real libc: the VFS cap is 512 KiB and a static glibc
  `hello` is ~800 KiB; `read_file` also allocates the full cap, and the kernel
  heap is 2 MiB, so file loading and these first libc syscalls are next.
  **Slice 3 (real libc):** `vfs::read_file_limit(cwd, path, limit)` reads a
  file with a caller-specific bound (the general `MAX_FILE_BYTES` policy is
  unchanged) and allocates exactly the file length; `sys_spawn_linux_path`
  uses a 1 MiB bound, so the ~800 KiB static glibc binary loads. Added
  `fstat` (a 128-byte aarch64 `struct stat`, `S_IFCHR|0620` for fds 0-2),
  `ioctl(TIOCGWINSZ)`, `writev`, `close`, `lseek` (`-ESPIPE`),
  `readlinkat` (`-ENOENT`) and `sched_yield`. An opt-in
  `linux-abi-trace` feature logs each still-unsupported number once (capped).
  Verified in QEMU on a clean boot: `linux-hello` (42), `linux-rw` (7) and a
  real **static glibc** `linux-libc` (`hello from libc`, 11); with the trace
  build the glibc run produces zero ENOSYS lines. Still missing: an fd table
  with `openat/read/close` of real files, signal delivery, thread creation,
  and ET_DYN relocation (link with `-no-pie`).
  **Slice 4 (real files):** file descriptors are the Silver VFS's own (it
  allocates from 3, reserving 0-2 for stdio), so the Linux side has no second
  table and `vfs::close_task` already reaps them on exit. Added
  `openat`, `read`, `close`, `lseek`, `fstat`, `newfstatat` and `getdents64`.
  Supporting VFS additions: `open_dir`, `fd_len`, `fd_kind`, `fd_path`,
  `lseek`, `dir_entries_from_fd`/`advance_dir_fd` (the descriptor offset
  advances only by the entries serialized into the caller's buffer) and
  `read_file_limit`. The Linux view is read-only (`-EROFS` for write intents).
  Verified with static glibc: `linuxrun /sfs/linux-libc` prints
  `hello from libc`, `readme!!`, `entries=5` and exits 0 (open/read/close and
  opendir/readdir over `/sfs`).
  **Slice 5 (fd process model + small syscalls):** Linux descriptors are now a
  real per-process table (`src/linux_fd.rs`) instead of direct VFS fds:
  0/1/2 are console handles and can be redirected with `dup2`, `/dev/null`,
  `/dev/zero`, `/dev/full` and `/dev/urandom` are synthetic devices, and
  entries 3+ wrap Silver VFS handles (released by `vfs::close_task` on exit;
  the table is dropped in `exit_current_with_status`/`kill_task`).
  Added `dup`/`dup3`, `fcntl` (F_DUPFD/F_DUPFD_CLOEXEC and flag no-ops),
  `getcwd`, `chdir`, `nanosleep`/`clock_nanosleep` (scheduler sleep),
  `clock_getres`, `gettimeofday`, `getrlimit`, `getuid`/`geteuid`/`getgid`/
  `getegid`, `sched_getaffinity` and `prctl`. The glibc test now also checks
  `fstat` size, `/dev/null` stdout redirection, `/dev/urandom`, `chdir`/
  `getcwd` and `nanosleep`, still exiting 0 with the console output above.
  Still open: per-task read scope, signals/threads, `pipe`/`poll`/`eventfd`,
  ET_DYN.
  **Slice 6 (`execve`):** `execve` replaces the current Linux image without
  changing the task id, capabilities, cwd or fd table. The new space/user
  stack are built while the old space is current, the CPU switches TTBR0, and
  only then is the old space released — `destroy_task_space` refuses a space
  that is still current. Teardown was factored into
  `SpaceResources`/`release_space_resources` so the superseded and a dead
  task's space share one release path (mmap, heap, tables, private page, DMA,
  images). The trampoline is retargeted by writing ELR (frame word 31) and
  SP_EL0. New test `tests/linux/chain.c` execs `/sfs/linux-hello`; QEMU keeps
  task 7, prints `hello from linux abi` and exits 42. Three consecutive execs
  followed by the glibc test and `health` show no frame/DMA leak (used 2171 of
  31615 frames, leaked 0).
  **Slice 7 (`fork` + `wait4`):** `clone(SIGCHLD)` with `stack == 0` forks the
  current Linux task. The address space is deep-copied eagerly: image runs,
  heap pages, mmap regions (now carrying their `prot`), private page and user
  stack get fresh frames; the fd table and cwd are duplicated
  (`vfs::dup_owner` clones the same fd numbers, `linux_fd::fork_table` copies
  the targets). The child's kernel stack starts with a copy of the parent's
  SVC frame whose `x0` is zero, and `silver_fork_bootstrap` restores the full
  register bank, so the child returns 0 from `clone` on its own stack while the
  parent gets the pid. Failed builds are unwound by `ForkParts`' `Drop`
  (space, private page, stack, heap, mmap, image). `wait4` matches children by
  ppid (positive pid or any), blocks through the existing process wait queue,
  reaps the zombie and writes `(status & 0xff) << 8`; `WNOHANG` returns 0.
  `tests/linux/fork.c` forks, the child execs `/sfs/linux-hello`, the parent
  waits and checks `WEXITSTATUS == 42`; QEMU prints the greeting, `parent saw
  child 42`, and the parent exits 0. Three consecutive forks plus the glibc
  test and `health` stay at 2171/31613 frames with no zombies or DMA leaks.
  Console writes from forked Linux children are allowed through
  `write_user_output_shared` (the Silver console owner is a single task; a
  process tree shares the terminal). Threads (`CLONE_VM`) are still `-ENOSYS`
  and COW is a later optimisation.
  **Slice 8 (`pipe2` + `ppoll`):** a pipe is a shared
  `Arc<Mutex<PipeState>>` (4 KiB deque + read/write endpoint counts) in
  `linux_fd`. `fork`/`dup` clone the `Arc` and bump an endpoint count;
  `close`/task teardown decrement it, so a read returns EOF only after every
  write end is gone. A blocking read/write sleeps 1 ms while it has no data /
  no space (the lock is dropped around the sleep), and `write` returns
  `-EPIPE` with no reader. `ppoll` (up to 32 fds, `POLLIN`/`POLLOUT`/
  `POLLHUP`/`POLLNVAL`, timespec timeout or 60 s cap for an infinite wait)
  re-checks readiness after each sleep. `tests/linux/pipe.c` forks, the child
  writes `ping`, the parent `ppoll`s and reads it; QEMU prints `pipe: ping`
  and both tasks exit 0. Three consecutive runs plus the glibc test and
  `health` stay at 2171/31612 frames with no zombies, open files or DMA
  leaks.
  **Slice 9 (`eventfd2`, `memfd_create`, `futex`):** eventfd is a shared
  `u64` counter (`read` drains it, `write` adds, `ppoll` reports `POLLIN`
  when non-zero; `EFD_NONBLOCK` returns `-EAGAIN`), memfd is an anonymous
  in-memory file with read/write/lseek/fstat and a shared offset, and
  `FUTEX_WAIT` validates the user word then sleeps for the timeout (10 ms
  default) reporting `-ETIMEDOUT`; `FUTEX_WAKE` returns zero waiters. Without
  threads a contended wait cannot be woken, so this is a compatibility shim.
  Anonymous `mmap` now rejects non-`MAP_ANONYMOUS` requests with `-ENODEV`
  instead of silently returning zeroed memory.
  **Slice 10 (signals):** `src/linux_signal.rs` keeps per-task dispositions
  (aarch64 `struct sigaction`: handler/flags/restorer), the blocked mask and
  pending bits. `rt_sigaction`/`rt_sigprocmask` round-trip, `kill`/`tgkill`
  apply default actions (`SIGKILL` and default-terminating signals terminate;
  ignored ones do nothing; `pid <= 0` targets the caller), and a child's exit
  raises `SIGCHLD` in a parent that has a signal table (`SIG_IGN` suppresses
  it). `wait4` clears the pending `SIGCHLD`. Custom handlers are stored but
  **not invoked**: `rt_sigframe` construction/`rt_sigreturn` is still open.
  Tests: `linux-ipc` prints `ipc ok`, `linux-signal` prints `signal ok`
  (exercising sigaction/sigprocmask, wait/SIGCHLD and SIGKILL of a sleeping
  child). Running all six Linux tests plus the glibc test and `health` stays
  at 2171/31608 frames, zero zombies, zero open files, zero DMA leaks.
  **Slice 11 (ET_DYN + PT_INTERP):** `load_linux_elf_at` accepts `ET_DYN` and
  adds a caller-chosen bias; `load_linux_program` picks `MAIN_BIAS`
  (0x1000_0000) for PIE, parses `PT_INTERP`, reads the interpreter from the
  VFS and attaches its runs at `INTERP_BIAS` (0x2000_0000). `LinuxImage`
  tracks `program_entry` (for `AT_ENTRY`), `interp_base` (for `AT_BASE`) and
  `interp_path`; `entry` becomes the interpreter entry, so the trampoline and
  `execve` start there. `fork` duplicates both. Test: `tests/linux/piemain.c`
  is `-pie --dynamic-linker=/sfs/ld-test` and `tests/linux/interp.c` is a
  reloc-free ET_EXEC interpreter that walks auxv to `AT_ENTRY` and branches;
  `linuxrun /sfs/linux-pie` prints `pie main` and exits 7. A real libc
  interpreter still needs file-backed `mmap`/`mprotect` and a bigger rootfs.
  Running pie, signal, pipe, glibc and `health` together stays at 2171/31608
  frames with no zombies/open files/DMA leaks.
  **Slice 12 (16 MiB rootfs):** the SFS image is 16 MiB with 512 inodes;
  `scripts/make_sfs.py` defaults to that geometry and the new
  `scripts/sfs_resize.py` grows an existing image while preserving every file
  (inode numbers and directory entries are stable, block trees are copied).
  `tools/linux-test.sh` grows `disk.img` on first use and installs
  `tests/linux/bigfile.c`, which reads a 3 MiB payload (`big.bin`) in 64 KiB
  chunks and prints `rootfs ok`.
  Three real bugs surfaced and were fixed with it:
  * `sfs::load` copied only the first 2048 sectors (1 MiB) into RAM no matter
    how big the device was; it now reads every sector up to
    `MAX_IMAGE_SECTORS = 32768` (16 MiB).
  * `sfs_format::fsck` used a fixed `MAX_FILE_BLOCKS` array and rejected any
    filesystem with more data blocks than one file may own (so every image
    above ~8.25 MiB failed to mount); it now tracks one bit per data block in
    a heap `Vec`.
  * `flush_bitmap` wrote only the first bitmap sector. Above 4096 blocks the
    remaining allocation bits never reached the disk, so a later writer
    reallocated live blocks (the first 16 MiB boot produced a
    `scopeprobe.elf`/`linux-bigfile` cross-link). It now writes every bitmap
    sector, verified by a host test and an offline `sfs_fsck.py` after boot.
  The kernel heap grew to 16 MiB (a real `libc.so.6` is about 2.2 MiB), the
  VFS/Linux exec cap to 8 MiB, and `copy_from_user`/`copy_to_user` now chunk
  into 4 KiB validation windows instead of rejecting any multi-page buffer, so
  a normal 64 KiB `read(2)` works. Frame use after the full Linux test series
  plus `health`: 9595/31609, zero zombies/open files/DMA leaks. The remaining
  gate for a real `ld.so`/libc is file-backed `mmap` and a real `mprotect`.
  **Slice 13 (file-backed `mmap`, `mprotect`, real `ld.so`):**
  `linux_mmap` now takes the full six arguments. `MAP_PRIVATE` file mappings
  read the requested range through a new `linux_fd::pread`
  (`vfs::pread_with`, no fd-offset change) and copy it into fresh private
  frames; the tail past EOF stays zero. `MAP_FIXED` replaces any existing
  mapping in the range, which is what glibc does after reserving a library
  span with `PROT_NONE`. `mprotect` rewrites PTEs in place, splits mapping
  bookkeeping at both range edges, and also handles the main executable and
  interpreter images below the mmap window (their `PageKind`s are updated so
  `fork` keeps the final protections). `mmap`/`munmap` now accept gaps and
  multi-region ranges.
  Auxv gained `AT_EXECFN` (a stack pointer to argv[0], filled by
  `build_linux_stack`) so `$ORIGIN` works without `/proc/self/exe`; the spawn
  environment also sets `LD_LIBRARY_PATH=/sfs`.
  Two `mmu` bugs surfaced: `protect_user_pages_at` indexed the image range
  with the mmap-window `(va - DRAM_BASE)` formula, which underflowed at
  `0x1000_0000` and panicked the kernel; it now walks L1/L2/L3 generically
  like `install_linux_run`. `scripts/sfs_inject.py` also had an off-by-one
  when growing a directory (`len(dir_blocks) + 1`), leaving a hole that the
  packed kernel walker stops at; the root had more than 16 entries for the
  first time, which exposed it.
  Test: `tools/linux-test.sh` injects the toolchain's `ld-linux-aarch64.so.1`
  and `libc.so.6`, patches `tests/linux/dynhello.c` to
  `/sfs/ld-linux-aarch64.so.1` with RUNPATH `$ORIGIN`, and installs
  `linux-dyn`. QEMU prints `dynamic hello` and exits 0: file-backed libc
  mapping, relocations, image RELRO `mprotect` and libc startup all work.
  `linux-mmapfile` covers file contents, a writable page, and a `MAP_FIXED`
  replacement over a `PROT_NONE` reservation. Full series plus `health`:
  9596/31606 frames, zero zombies/open files/DMA leaks; offline fsck clean.
  Remaining Linux gaps: signal frames/`rt_sigreturn`, COW fork, threads, a
  per-task read scope, and writable/shared file mappings.
  **Slice 14 (signal frames and `rt_sigreturn`):** `src/linux_signal.rs` now
  builds a real AArch64 `rt_sigframe` on the user stack (`siginfo` 128 B +
  `ucontext` 4544 B with `sigcontext`), stores the interrupted x0-x30/SP/PC/
  PSTATE, and enters the handler with `(sig, siginfo, ucontext)`. Delivery runs
  in the SVC return path, so the target is always the current task; ignored
  and default signals are resolved while scanning, default-terminating ones
  exit, and a user handler blocks its own signal (plus `sa_mask`) unless
  `SA_NODEFER`. `SA_RESETHAND` resets to `SIG_DFL`.
  `rt_sigreturn` restores the register bank, SP, PC/PSTATE and `uc_sigmask`.
  AArch64 glibc does not pass `SA_RESTORER`; the kernel therefore supplies a
  cached RX page containing `mov x8, #139; svc #0`, while an explicit restorer
  is honored when `SA_RESTORER` is set. `rt_sigaction` now stores and returns
  `sa_mask` too. `wait4` leaves `SIGCHLD` pending when a user handler is
  installed, so the handler runs after the reap.
  Tests: `linux-sighandler` covers `SA_SIGINFO`, siginfo fields, the restorer
  path and a blocked-then-unblocked signal (`sigframe ok`); `linux-dynsig`
  uses real glibc `sigaction`/`raise` (`dyn signal 10`). Remaining gaps:
  asynchronous delivery to a task already running in EL0, `sigaltstack`,
  COW fork and threads.
  **Slice 15 (async signals + sigaltstack):** the IRQ trampoline now calls
  `silver_irq_signal_return` with the saved EL0 bank before publishing
  ELR/SPSR, so a pending handler is entered on the next timer tick even when
  the task never issues a syscall. Only EL0t contexts are eligible; the hook
  is a no-op for kernel interrupts and native tasks. `kill` wakes a
  `Sleeping`/`Blocked` target through `scheduler::wake_for_signal` so the
  signal interrupts sleep/blocked waits. `sigaltstack` stores a per-task
  alternate stack; `SA_ONSTACK` frames are built at its top, and a nested
  signal falls back to the normal stack (`on_altstack` is cleared by
  `rt_sigreturn`). Test `linux-asyncsig` forks, has the child kill the parent
  while the parent busy-loops in EL0, then raises `SIGUSR2` to an
  `SA_ONSTACK` handler and checks that SP is inside the registered buffer:
  `async sig ok`. Remaining gaps: COW fork, threads, a per-task read scope.
  **Slice 16 (COW fork):** `fork` shares every user frame instead of copying.
  The frame allocator now keeps a `u16` refcount per page; `free_*` releases an
  owner and only clears the bitmap for the last one, and `share_*` adds an
  owner after validating the generation. `LinuxImage::share` shares image runs
  and turns writable pages into `PageKind::Cow`; `create_task_space(..., cow)`
  maps stack/private read-only without zeroing; new
  `map_shared_user_pages_at`/`map_shared_user_heap_pages` map shared runs
  without clearing them. The parent is switched to read-only in the same
  operation.
  A data-abort vector now saves the full EL0 bank and calls
  `silver_handle_user_fault`; `scheduler::cow_fault` locates the image run,
  stack, private page, heap page or mmap region, copies the whole containing
  run, remaps it with the original permissions and drops the old reference.
  Kernel `copy_to_user` also runs `cow_prepare` so `wait4`, `read` and signal
  frames can write into a COW stack page.
  Two more bugs found: the sync vector used x11 as scratch before saving the
  user bank (a COW return clobbered live x11), and the first fork version left
  `ForkParts` owning the shared stack/private page, double-freeing them at
  teardown.
  Test `linux-cowfork` writes distinct bytes to image data, a 1 MiB BSS
  array, an anonymous mapping, `brk` and a real stack array from parent and
  child, verifies isolation and repeated writes: `cow fork ok`. Three
  consecutive runs plus the full Linux series and `health` stay at
  9596/31586 frames with zero zombies/open files/DMA leaks; offline fsck
  clean. Remaining gaps: threads (`CLONE_VM`).
  **Slice 17 (per-task read scope):** every Linux task carries a read prefix
  (default `/sfs`); native tasks carry `None`. `openat`, `newfstatat`,
  `chdir` and `execve` resolve the path first and return `-EACCES` when it is
  outside the prefix. `/dev/*` synthetic devices remain reachable before the
  scope check. The prefix is copied across `fork` and preserved by `execve`.
  Test `linux-scope` denies `/bin/hello` and `/bin`, allows
  `/sfs/readme.txt` and `chdir /sfs`, and prints `scope ok`.
  **Slice 18 (threads):** `clone(CLONE_VM|CLONE_THREAD|...)` creates a task
  that shares the leader's TTBR0 and memory but owns only a fresh kernel stack
  and the caller-provided user stack. `TaskContext` gained `tls` and the
  context switch saves/restores `TPIDR_EL0`; `CLONE_SETTLS` sets it per thread.
  `getpid` returns the thread-group id, `gettid` the task id.
  Futex is real: a per-address/tgid waiter table, `FUTEX_WAIT` blocks until
  `FUTEX_WAKE`, a changed word or a timeout; `FUTEX_WAKE` wakes sleepers.
  `CLONE_PARENT_SETTID`/`CHILD_SETTID` publish the tid before the child runs,
  `CLONE_CHILD_CLEARTID` clears it and wakes the join futex on exit, and thread
  records are removed in the reaper instead of consuming the zombie cap.
  Supporting fixes found by glibc: `faccessat` (read-only, no `W_OK`),
  `madvise` as a no-op, and `getrlimit(RLIMIT_STACK)`/`prlimit64` returning
  8 MiB so pthread does not request a 512 MiB stack.
  Tests: `linux-threads` (freestanding CLONE_VM + TLS + futex) prints
  `threads ok`; `linux-pthread` (real glibc pthread_create/join) prints
  `pthread 42`. Full series plus repeated thread tests and `health` stay at
  9596/31585 frames, zero zombies/open files/DMA leaks; offline fsck clean.
  Limitations: fd table, heap and mmap bookkeeping are still leader-owned, and
  `exit_group` with live workers is not complete.
  **Slice 19 (shared thread-group resources):** Linux fd operations resolve
  the group leader's descriptor table (`fd_table_owner`), and VFS open/read/
  write/close/lseek/fstat/getdents map the owner too, so a worker shares fds.
  `brk`, anonymous/file `mmap`, `munmap`, `mprotect`, COW faults and the signal
  trampoline run on the group leader, so heap/mmap/image state has one owner.
  `exit_group` from the leader marks every sibling Dead, yields until the
  reaper removes them, then exits and releases the shared space; a sleeping
  worker is killed. Tests: `linux-pthread` now mallocs and prints from the
  worker (`worker x`, `pthread 42`); `linux-pthread-exit` leaves a sleeping
  worker and returns from main (`exit group ok`). Full series plus repeated
  runs and `health`: 9596/31584 frames, zero zombies/open files/DMA leaks.
  Remaining: none for threads beyond the usual single-address-space caveats.
  **Slice 20 (group exit and signal dispositions):** `exit_group` from a
  non-leader now kills every group member, gives the leader record the group
  status and lets the reaper release the shared space. A task killed from
  another CPU cannot run its own exit path, so the reaper returns a console
  it owned; without that the shell stayed alive while its output went to the
  dead owner. Blocking transitions (`sleep`, `wait_irq`, wait-queue blocks)
  no longer overwrite `Dead`, so a task marked dead mid-syscall is still
  reaped. Signal dispositions live in the thread-group leader's table, while
  blocked/pending/alternative-stack state stays per thread. Tests:
  `linux-exitgroup` (worker `exit(0)` kills a sleeping main, prints
  `worker exiting`) and `linux-groupsig` (worker installs SIGUSR1, main
  raises it, prints `group signal ok`). Full series plus repeats and
  `health`: 9596/31583 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 21 (Unix sockets):** `socketpair(2)` creates an `AF_UNIX` stream
  pair whose shared state tracks directional message queues, per-end descriptor
  reference counts and read/write half-closes. `sendmsg`/`recvmsg` carry
  `SCM_RIGHTS` descriptors; the receiver installs them under fresh numbers and
  truncation closes them with `MSG_CTRUNC`. Plain `read`/`write`, `sendto`/
  `recvfrom`, `shutdown`, `getsockname`/`getpeername`, `setsockopt`/
  `getsockopt` (`SO_TYPE`, `SO_ERROR`, buffers) and `fcntl(F_GETFL/F_SETFL)`
  with `O_NONBLOCK` work; `ppoll` reports `POLLIN`/`POLLOUT`/`POLLHUP`. The fd
  table grew to 256 slots. Tests: `linux-unixsock` (freestanding byte stream,
  SCM_RIGHTS eventfd round trip, EAGAIN and EOF) prints `unix socket ok`;
  `linux-sockets` (real glibc `socketpair`/`sendmsg`/`recvmsg` with `CMSG_*`,
  `poll`, `O_NONBLOCK`, `shutdown`) prints `glibc socket ok`. Full series plus
  repeats and `health`: 9596/31578 frames, zero zombies/open files/DMA leaks;
  fsck clean. Named `AF_UNIX` sockets still need a writable runtime directory.
  **Slice 22 (shared memfd mappings):** `memfd_create` files can be sized with
  `ftruncate`/`fallocate` and mapped writable with `MAP_SHARED`. The file owns
  one contiguous physical run; every mapping (including another process after
  `SCM_RIGHTS`) points at the same frames, and `read`/`write`/`pread`/`pwrite`
  go through them. `pread64`/`pwrite64` were added. `MmapRegion` gained a
  `SharedMapping` handle: unmapping or task teardown must not free memfd
  frames, `fork` keeps shared windows writable instead of COW-protecting them,
  and a write fault on a read-only shared window kills the task rather than
  copying. The run is released with the last descriptor/mapping. Test:
  `linux-memfd` maps, writes through mapping and file, forks a child that
  writes through its own window, observes the child's bytes after `waitpid`,
  unmaps and remaps without losing data; prints `shared memfd ok`. Full series
  plus repeats and `health`: 9596/31580 frames, zero zombies/open files/DMA
  leaks; fsck clean.
  **Slice 23 (named Unix sockets):** `socket(AF_UNIX)` creates an unbound
  endpoint; `bind` reserves a filesystem or abstract path in a global
  registry, `listen` arms the backlog, `connect` joins a listening path and
  turns the caller's descriptor into the client end, and `accept`/`accept4`
  return the server end. `getsockname`/`getpeername` emit the Linux
  NUL-terminated address, connections respect `O_NONBLOCK`, duplicates and
  forks keep per-end reference counts, and a closed duplicate no longer
  unbinds a live listener. No socket file is created on disk yet.
  **Fork TLS fix:** a dynamic glibc child could fault at `-1616` because
  `fork` copied the parent's cached `TaskContext::tls`, which is only
  refreshed on a context switch. The current task's live `TPIDR_EL0` is now
  read directly at fork. Tests: `linux-namedsock` prints `named socket ok`;
  with the fix, 14 consecutive `linux-memfd` runs (glibc `fork` + TLS) and the
  full series are clean. Full series plus repeats and `health`:
  9596/31578 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 24 (writable scratch tree):** the root RamFs already had `/tmp`;
  Linux tasks may now read and write it while SFS stays read-only. `openat`
  with `O_CREAT`/`O_EXCL`/`O_TRUNC`/`O_APPEND` creates, truncates and appends
  through the VFS, and `mkdirat`, `unlinkat` (including `AT_REMOVEDIR`) and
  `renameat`/`renameat2` work there. `RamFs` gained a replacing `rename`, and
  the VFS scope check allows the scratch prefix. Linux spawn supplies
  `TMPDIR=/tmp` and `XDG_RUNTIME_DIR=/tmp`, the locations Wayland/D-Bus
  toolkits use for sockets and lock files. Test: `linux-scratch` checks
  `getenv`, creates a lock file with `O_EXCL`, verifies `EEXIST`, appends,
  `stat`s the size, renames over the destination, writes inside a runtime
  directory, removes everything, and confirms `/sfs` still returns `EROFS`;
  prints `scratch ok`. Full series plus repeats and `health`:
  9596/31577 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 25 (event loop primitives):** `epoll_create1`/`epoll_ctl`/
  `epoll_pwait`/`epoll_pwait2` provided level-triggered readiness for every
  pollable target (pipes, eventfd, sockets, listeners, timerfd), with
  `EPOLLERR`/`EPOLLHUP` always reported and a bounded infinite timeout like
  `ppoll`. `timerfd_create`/`settime`/`gettime` implement one-shot and
  interval timers over the 100 Hz scheduler tick, including absolute
  deadlines and expirations on `read`. Test: `linux-eventloop` (glibc
  `epoll_create1`, three registered descriptors, timeout with no events,
  pipe readiness, a 50 ms timer, an eventfd, `EPOLL_CTL_DEL`, disarm and
  `gettime`) prints `event loop ok`. Full series plus repeats and `health`:
  9596/31575 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 26 (Wayland wire protocol):** a freestanding compositor/client pair
  speaks the Wayland wire format over a named `AF_UNIX` socket. The server
  implements `wl_display.get_registry`/`sync`, sends `wl_registry.global` for
  `wl_compositor`/`wl_shm`, tracks `bind`, handles
  `wl_compositor.create_surface`, `wl_shm.create_pool` (receiving the memfd
  with `SCM_RIGHTS`), `wl_shm_pool.create_buffer`, `wl_surface.attach`/
  `damage`/`commit`, maps the pool `MAP_SHARED` and verifies the client's
  ARGB8888 pixels before acknowledging the final sync. The client parses
  globals, binds, draws into a memfd, commits and waits for the second
  `wl_callback.done`; the parent only prints `wayland ok` after the
  compositor read the shared buffer. Full series plus repeats and `health`:
  9596/31575 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 27 (D-Bus auth and messaging):** `getsockopt(SO_PEERCRED)` returns a
  `struct ucred` (pid/uid/gid 0, the root Linux personality) for any Unix
  socket, which is what EXTERNAL authentication verifies. A freestanding
  daemon/client pair performs the NUL-prefixed `AUTH EXTERNAL 30` handshake,
  `OK <guid>`, `BEGIN`, then exchanges a binary
  `org.freedesktop.DBus.Hello` method call and a `method_return` with
  `REPLY_SERIAL` and an `s` body. The parser handles the 8-byte-aligned
  header-field array, variant signatures (`s`/`o`/`u`/`g`) and the padded
  body. Test: `linux-dbus` prints `dbus ok`; the glibc socket test also checks
  `SO_PEERCRED`. Full series plus repeats and `health`: 9596/31575 frames,
  zero zombies/open files/DMA leaks; fsck clean.
  **Slice 28 (D-Bus UNIX_FD and signals):** the D-Bus test now negotiates
  `NEGOTIATE_UNIX_FD`/`AGREE_UNIX_FD`, sends a `SendFd` method call with
  `UNIX_FDS=1` and an `h` body argument backed by a memfd passed over
  `SCM_RIGHTS`, has the daemon map and verify the payload, returns a second
  memfd in the `method_return`, and emits a `Changed` signal with an `s` body.
  The client validates the reply serial, the received descriptor's contents
  (mapping it `MAP_SHARED`) and the signal. Full series plus repeats and
  `health`: 9596/31575 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 29 (real libwayland):** a real aarch64 `libwayland-client.so.0`
  (with `libffi.so.8`) now runs on the ABI. `linux-wlreal` forks and execs
  `wlclient`, which connects to the mini compositor, binds
  `wl_compositor`/`wl_shm`, builds a memfd ARGB buffer, commits and
  round-trips; the compositor verifies the shared pixels. Two bugs were found:
  `fstat` returned the descriptor number as `st_ino`, so `ld.so` treated every
  library as the same file via `(st_dev, st_ino)` and reused the first link
  map (now a stable FNV-1a hash of the VFS path, and `newfstatat` uses the
  same); and the Wayland header order is object/opcode/size, not the
  size/opcode used by the earlier self-consistent test. Vendored libs and
  headers live in `third_party/wayland-aarch64` (MIT). Full series plus
  repeats and `health`: 9596/31575 frames, zero zombies/open files/DMA leaks;
  fsck clean.
  **Slice 30 (xdg-shell):** the mini compositor advertises `xdg_wm_base` and
  handles `get_xdg_surface`/`get_toplevel`/`ack_configure`, sending
  `xdg_toplevel.configure` (8x8, empty states) and `xdg_surface.configure`.
  The real libwayland client binds the shell, creates the xdg surface and
  toplevel, does a bufferless commit, waits for configure, acknowledges the
  serial and only then attaches its memfd buffer. `wayland-protocols` 1.50
  xdg-shell headers/generated code and its license are vendored. Full series
  plus repeats and `health`: 9596/31575 frames, zero zombies/open files/DMA
  leaks; fsck clean.
  **Slice 31 (wl_seat and wl_output):** the compositor advertises `wl_output`
  (geometry/mode/done) and `wl_seat` (pointer+keyboard capabilities). After the
  surface is created the real libwayland client calls get_keyboard/
  get_pointer; the compositor sends `wl_keyboard.keymap` with a memfd over
  `SCM_RIGHTS`, then `wl_keyboard.enter`, and `wl_pointer.enter`/`motion` with
  24.8 fixed coordinates. The client checks the keymap marker and the pointer
  position. The keymap descriptor shares the writer's file description, so
  the client rewinds (and the compositor seeks to zero) before reading. Full
  series plus repeats and `health`: 9596/31575 frames, zero zombies/open
  files/DMA leaks; fsck clean.
  **Slice 32 (stock wayland-info):** the unmodified `wayland-info` binary from
  `wayland-utils` 1.3.0 runs against the mini compositor. It is linked with
  the vendored libwayland-client and libdrm; patchelf rewrites the interpreter
  and rpath at test-build time. It enumerates wl_compositor, wl_shm (AR24
  format), xdg_wm_base, wl_output (Werewolf/Panel, 1024x768@60, current
  preferred) and wl_seat (pointer keyboard), then exits 0. A dedicated
  harness (`wayland_info.c`) serves until EOF and prints `wayland-info ok`.
  Full series plus repeats and `health`: 9596/31575 frames, zero zombies/open
  files/DMA leaks; fsck clean.
  **Slice 33 (frame callbacks and buffer release):** the real client creates
  two buffers from one shm pool (offsets 0 and 2048), draws two distinct
  patterns and commits three alternating frames. It requests a
  `wl_surface.frame` callback per frame and blocks until both
  `wl_callback.done` and `wl_buffer.release` arrive before drawing the next
  one. The compositor verifies either pattern on every commit, tracks the
  commit count and only reports success after three frames. Full series plus
  repeats and `health`: 9596/31575 frames, zero zombies/open files/DMA leaks;
  fsck clean.
  **Slice 34 (larger SFS and metadata):** the rootfs grows to 32 MiB with 1024
  inodes. The kernel heap grows to 48 MiB (12288 pages) because the whole SFS
  image is loaded into it, and `MAX_IMAGE_SECTORS` rises to 65536. Scripts,
  `--linux-tests` and `tools/linux-test.sh` default to 32 MiB / 1024.
  The big-file test now writes and verifies an 8 MiB payload (the SFS block
  tree's maximum), covering the enlarged bitmap/inode metadata end to end.
  Full series plus repeats and `health`: 21884/31575 frames (heap included),
  zero zombies/open files/DMA leaks; fsck clean.
  **Slice 35 (two clients) and status refresh:** `linux-wlreal` now forks two
  real `libwayland` clients; the harness accepts both, resets the per-client
  surface namespace, serves each (three animated frames each) and reports
  `libwayland 2 clients ok`. Added `docs/STATUS.md`, a fresh project snapshot
  (architecture, capabilities, limits table, test matrix, build commands and
  roadmap) linked from the README. Full series plus repeats and `health`:
  21884/31575 frames, zero zombies/open files/DMA leaks; fsck clean.
  **Slice 36 (per-surface focus):** keyboard and pointer focus is now driven by
  the surface lifecycle. `get_keyboard` only delivers the keymap;
  `get_pointer` only records the object. On the first commit with a buffer the
  compositor sends `wl_keyboard.enter` and `wl_pointer.enter`/`motion` for that
  surface and marks it focused; `wl_surface.destroy` sends `wl_keyboard.leave`
  and `wl_pointer.leave` and drops focus. The real client checks enter/leave
  and leaves the surface only after its frames. Two concurrent clients still
  pass. Full series plus repeats and `health`: 21884/31575 frames, zero
  zombies/open files/DMA leaks; fsck clean.
  **Slice 37 (64 MiB frame-backed SFS):** the image cap rises to 64 MiB with
  2048 inodes, and the kernel heap returns to 16 MiB. The SFS image was already
  allocated in contiguous frames (not the heap), so this uses the frame
  budget directly; `MAX_IMAGE_SECTORS` is now 131072 and scripts/test defaults
  are 64 MiB / 2048. The 8 MiB payload test still passes. This is a capacity
  step, not demand streaming: the whole image is still resident. Full series
  plus repeats and `health`: 21884/31575 frames, zero zombies/open files/DMA
  leaks; fsck clean.
  **Slice 38 (streaming SFS):** `src/sfs.rs` is now a streaming, read-only
  backend. The kernel owns virtio-blk, reads the superblock at mount and then
  serves inodes, directories, indirect tables and file data from a 512-block
  (256 KiB) write-through cache. The whole image is no longer allocated or
  read at boot; `MAX_IMAGE_SECTORS` and the image buffer are gone. `fstat`-style
  usage scans the bitmap and inode table on demand. All Linux tests, real
  libwayland, wayland-info, D-Bus and the 8 MiB payload read correctly;
  resident frames dropped from 21884 to **5500/31515**. SFS mutations are
  disabled until the block-allocation/write path is ported; `/tmp` stays
  writable. Full series plus repeats and `health`: zero zombies/open
  files/DMA leaks; fsck clean.
  **Slice 39 (writes on streaming SFS, wl_output v2):** the streaming cache
  gained block allocation/free, inode allocation, directory add/remove and
  file write/truncate. Creating, writing, growing via indirect/double-indirect
  tables, `mkdir`, `rm` and `rmdir` work through the cache and persist to the
  device; `rename`/`replace` remain disabled. The shell test writes, reads,
  nests and removes files (fsck clean afterwards). Wayland advertises
  `wl_output` v2 and sends `scale(2)`; the real client binds v2 and checks it.
  Full series plus repeats and `health`: 5500/31515 frames, zero zombies/open
  files/DMA leaks; fsck clean.
  **Slice 40 (SFS rename/replace, wl_surface.enter):** the streaming backend
  gained `rename` and `replace`. Same-parent renames rewrite the directory
  entry in place; cross-directory moves add the destination entry and roll it
  back if the source entry cannot be detached; directories are checked
  against moving into their own subtree. `replace` detaches the source name,
  re-points the destination entry at the source inode and releases the old
  destination (leaking blocks only if the final free fails, never the old
  file). QEMU tests cover file and directory moves, a rejected overwrite, a
  rejected subtree move, and replace-over-destination; fsck stays clean.
  Wayland adds `wl_surface.enter` and the real client verifies it alongside
  `wl_output` scale 2.
  **Slice 41 (triple-indirect files):** the inode's reserved word at offset 32
  is now a third indirect pointer. `src/sfs.rs` reads, allocates, truncates and
  frees the triple tree (outer 128×128 double tables), `MAX_FILE_BLOCKS`
  grows to about 1 GiB, and append allocation starts at the current EOF block
  instead of rescanning from slot zero. `scripts/sfs_inject.py`,
  `sfs_fsck.py` and `sfs_resize.py` understand the same tree. Tests: a 9 MiB
  payload injected offline is read byte-for-byte by a Linux test (`triple
  ok`) and by `wc`; appending two bytes to a file ending exactly at the first
  triple slot allocates the triple root, double table and leaf table, and
  grows the file to 8,456,194 bytes. Deleting that 9 MiB file now takes under
  a second because free bits are grouped per bitmap block (bitmap writes no
  longer scale with file size); fsck is clean before and after. Whole-file
  reads and `exec` remain capped at 8 MiB by `MAX_FILE_BYTES`.
  **Slice 42 (Wayland v4/v5 globals):** the mini compositor now advertises
  `wl_compositor` v4, `wl_output` v4 and `wl_seat` v5, and `wl_shm` lists both
  ARGB8888 and XRGB8888. `wl_output` sends scale, name and description before
  done; `wl_seat` sends its name; `wl_keyboard.repeat_info` is emitted for
  version-4 keyboards. The real libwayland client binds the higher versions
  and verifies name/description/repeat plus the existing scale/enter/input
  events; the stock `wayland-info` harness advertises the same versions and
  prints them. Both clients and `wayland-info` pass.
  **Slice 43 (xdg_toplevel states):** the compositor answers
  `set_maximized`, `unset_maximized`, `set_fullscreen` and
  `unset_fullscreen` with `xdg_toplevel.configure` state arrays
  (`MAXIMIZED`/`FULLSCREEN` or empty) followed by a matching
  `xdg_surface.configure` serial. The real libwayland client drives all four
  transitions, acks each serial and verifies the state array. Two concurrent
  clients still pass and `health` is clean.
  **Slice 44 (xdg_popup):** the compositor tracks a second surface and its
  xdg_surface, answers `xdg_surface.get_popup` with `xdg_popup.configure` plus
  an `xdg_surface.configure` serial, accepts `xdg_popup.grab` and delivers
  `popup_done` once the popup is mapped. The real client builds a positioner,
  verifies the 40/30/200/120 configure, commits the popup, grabs it and waits
  for `popup_done` before destroying the popup surface. Two concurrent clients
  pass; `health` stays clean.
  **Slice 45 (streaming Linux exec):** `linux_elf.rs` gained an `ElfSource`
  abstraction: the existing slice path is unchanged, and a new VFS source
  streams the main executable. Only the ELF header, program headers, `PT_INTERP`
  string and the (small) interpreter are buffered; each `PT_LOAD` is zeroed and
  then filled in 512-byte chunks from `vfs::read_at` directly into its frames.
  `LINUX_EXEC_LIMIT` rises to 32 MiB and `MAX_IMAGE_PAGES` to 8192. Unsigned
  plain ELFs no longer materialise the file for bundle verification; a manifest
  that claims a digest still reads the file and stays under 8 MiB. A temporary
  9.4 MiB aarch64 ELF (a binary-blob object linked into a tiny program) ran as
  `stream exec ok`, proving the >8 MiB path; the full Linux series, real
  libwayland and `wayland-info` still pass, and fsck/health stay clean.
  **Slice 46 (clipboard data-device):** the mini compositor advertises
  `wl_data_device_manager` v1. It tracks a data source and its mime list,
  answers `set_selection` with `data_offer`/`offer`/`selection`, and forwards
  `data_offer.receive(mime, fd)` to `wl_data_source.send` over `SCM_RIGHTS`.
  The real client offers `text/plain;charset=utf-8`, receives the selection,
  reads the text from the pipe and verifies `clipboard from werewolf`. The
  server IDs start at `0xFF000000` (the Wayland server range) and the test
  passes in both concurrent clients. `wayland-info` lists the new global.
  **Slice 47 (multiplexed compositor, cross-client clipboard):** the test
  compositor now keeps a `ClientState` per connection (all protocol object
  ids, surface/popup state, buffers, flags) and loads/saves it around each
  pollable `serve` call. `ppoll` drives both accepted sockets, so a client can
  receive events while the other is executing. The clipboard is shared:
  `set_selection` broadcasts a data offer to every client with a data device,
  and `data_offer.receive` forwards `wl_data_source.send` to the owner's fd
  even while the receiver is being served. `wayland_real` spawns a `source`
  and a `receiver` client; both still run the full surface/popup/frame test,
  then the source offers text and the receiver reads it. Server object ids are
  allocated per client from 0xFF000000. Both clients pass; health is clean.
  **Slice 48 (decoration and viewporter):** the test compositor advertises
  `zxdg_decoration_manager_v1` v2 and `wp_viewporter` v1. Decoration answers
  `get_toplevel_decoration` with client-side mode and echoes
  `set_mode`/`unset_mode` through configure events; viewporter accepts
  `get_viewport`, `set_source` and `set_destination`. The real client binds
  both, checks the decoration configure loop and applies a source/destination
  crop-scale before the frame loop. `wayland-info` lists both new globals.
  The protocol headers/code were generated with `wayland-scanner` from the
  upstream XML and checked into `third_party/wayland-aarch64`.
  **Slice 49 (wl_subcompositor):** the compositor advertises
  `wl_subcompositor` v1 and tracks a third surface as a subsurface. It accepts
  `get_subsurface`, `set_position`, `place_above`/`place_below` and
  `set_sync`/`set_desync`; the commit of the child surface is observed so the
  final check requires it. The real client builds a child surface under the
  toplevel, commits it, destroys it, and the multiplexed source/receiver
  clients plus `wayland-info` all pass.
  **Slice 50 (xdg-output):** the compositor advertises
  `zxdg_output_manager_v1` v3. `get_xdg_output` returns logical position
  (0,0), logical size (1024x768), name, description and a matching
  `wl_output.done`; the real client binds v3, verifies every event and tears
  the object down. The stock `wayland-info` harness advertises the same
  global and prints the logical region. Both multiplexed clients,
  cross-client clipboard, decoration/viewporter/subcompositor and
  wayland-info pass.
  **Slice 51 (xdg-activation):** `xdg_activation_v1` is advertised. The
  compositor issues a token on `xdg_activation_token_v1.commit` (after
  `set_app_id`/`set_surface`), accepts `activate(token, surface)`, and the
  real client verifies the token string before activating. `wayland-info`
  lists the global. All multiplexed client, clipboard and protocol tests stay
  green.
  **Slice 52 (fractional scale):** `wp_fractional_scale_manager_v1` v1 is
  advertised. `get_fractional_scale` returns `preferred_scale(240)` (2.0) for
  the surface; the client creates the object, verifies the event and destroys
  it. `wayland-info` lists the global alongside xdg-output and xdg-activation.
  All multiplexed client, clipboard and protocol tests stay green.
  **Slice 53 (presentation-time):** `wp_presentation` v3 is advertised and
  sends `clock_id(CLOCK_MONOTONIC)`. A feedback object gets `sync_output`
  followed by `presented` with a 60 Hz refresh and the vsync flag; the client
  verifies both and destroys the presentation object. `wayland-info` prints
  the clock id. All multiplexed client, clipboard and protocol tests stay
  green.
  **Slice 54 (relative pointer and constraints):** the compositor advertises
  `zwp_relative_pointer_manager_v1` and `zwp_pointer_constraints_v1`.
  `get_relative_pointer` answers with a relative motion (3.0, 4.0) and
  `lock_pointer` answers with `locked`; the client verifies both, destroys the
  objects and keeps running the rest of the test. `wayland-info` lists both
  globals. All multiplexed client, clipboard and protocol tests stay green.
  **Slice 55 (text input v3):** `zwp_text_input_manager_v3` is advertised.
  `get_text_input` creates a seat text-input object; enable sends `enter` for
  the focused surface, commit sends `commit_string("werewolf")` followed by
  `done(serial)`, and disable sends `leave`. The real client checks all three
  events. `wayland-info` lists the global. All multiplexed client, clipboard
  and protocol tests stay green.
  **Slice 56 (real GLib/GObject):** vendored the Arch Linux ARM `glib2`
  2.90.1 and `pcre2` 10.49 libraries and headers under
  `third_party/glib-aarch64`. `tests/linux/glib.c` prints the GLib version,
  instantiates a `GObject` subclass, formats a string and runs a `GMainLoop`
  with a timeout source. The dynamic binary runs as
  `linuxrun /sfs/glibtest` and prints `glib 2.90.1`, `gobject wolf-42`,
  `glib mainloop ok`; the existing libc/dynamic tests still run. This is the
  first real GLib app under the ABI, not a hand-written syscall test.
  **Slice 57 (uniform app launcher):** `linuxrun-<app>` now tries
  `/sfs/apps/<app>` and then `/sfs/<app>` before reporting failure, so
  `linuxrun-glibtest` and `linuxrun-linux-hello` work without a full path.
  Linux children get `WAYLAND_DISPLAY=wayland-test`, `XDG_RUNTIME_DIR=/tmp`,
  `XDG_DATA_DIRS=/sfs/share` and `XDG_CONFIG_HOME`/`XDG_CACHE_HOME` under
  `/tmp`; `glibtest` prints the inherited Wayland/runtime values. This is the
  launch shape `linuxrun-phosh` will use.
  **Slice 58 (GIO):** vendored `libgio`, `libgmodule`, `libz`, `libmount`,
  `libblkid`, `libsystemd` and `libgcc_s` alongside GLib. `tests/linux/gio.c`
  writes and reads `/tmp/gio-test.txt` through `g_file_set_contents`/
  `g_file_get_contents`, creates a `GFile`, and runs a `GMainLoop`; the guest
  prints `gio file ok`, `gio path /tmp/gio-test.txt`, `gio mainloop ok`.
  Both GLib and GIO tests run on the same 64 MiB image; 221 host tests pass.
  **Slice 59 (GTK3 probe):** a GTK3 3.24.52 demo (`gtk window shown`,
  `gtk mainloop ok`) runs under host `qemu-aarch64-static` with the Arch ARM
  GTK stack. In the guest, the 64 MiB image cannot hold the stack (~72
  libraries, ~75 MiB), so a temporary 256 MiB image was used. The dynamic
  loader gets through most libraries but then fails to map a 1.3 MiB segment:
  the per-task mapping policy still assumes a small number of large contiguous
  runs. This is a real mmap/VM slice, not a missing syscall. `scripts/
  sfs_inject.py` gained a free-block cursor so a 75 MiB bulk install takes
  ~1.7 s instead of tens of minutes. `wayland_real.c` also gained a
  `SERVE_ONLY` build that accepts one external client (the GTK harness).
  **Slice 60 (security audit follow-up):** the external audit's critical and
  high findings are closed. The kernel gained a ChaCha20 CSPRNG
  (`src/entropy.rs`, CNTVCT jitter + RNDR + boot state, re-keyed every
  1 MiB) behind `getrandom(2)`, `/dev/urandom`, `AT_RANDOM` and the new
  `SYS_RANDOM`; TLS lost its ticks-seeded xorshift. `embedded-tls` now
  requires BasicConstraints `cA=TRUE`, `keyCertSign` and `pathLenConstraint`
  on every non-anchor issuer, which closes the
  `[forgedLeaf, legitimateLeaf]` impersonation. `net_server` hands each
  client its own DMA window instead of one shared 1 MiB buffer, and ping
  replies follow the requester. Kernel-side: `parse_rights` cannot wrap,
  `exit_group`/`kill` clean futex/fd/signal/mailbox/VFS tables, `execve`
  refuses from a `CLONE_VM` sibling, `kill(2)` is bounded to the process
  tree + Linux tasks, the Linux write path enforces the manifest quota, and
  SFS re-runs a streaming fsck at mount. Also fixed: `RamFs::replace` no
  longer deletes the target before a failing rename, SFS mutations no longer
  treat an I/O error as a hole, and the boot-stub assembler flags live in
  the same `global_asm!` block that tests them (the old split invocations
  could fail an incremental build).
  Deliberately not changed: `copy_to_user`'s validate/copy pair is
  serialized by the kernel lock (every syscall, including `munmap`, holds
  it), so the reported CLONE_VM race is not reachable; `LX-7` (Linux tasks
  get a `/tmp` grant) remains a design decision.
- **Known issue:** the plain-HTTP path started within a fraction of a second of
  `howl ready` timed out once (DHCP race suspected; `wget` works seconds
  later). Wolfser now waits up to 6 s for a configured lease before an HTTP
  fetch, but this needs a deterministic QEMU test.
- **Desktop integration:** `./build_and_qemu.sh` (viewer on by default) adds
  the virtio-net card and the ivshmem device, incrementally builds the Qt
  viewer and starts it **hidden** with `--watch`; it shows a window only when
  the guest sets the window request, i.e. after `wolfser`. Nothing appears
  while the kernel boots, and the hidden viewer's stdin is `/dev/null` so it
  cannot take QEMU's serial input. While a page is fetched Wolfser paints a
  "LOADING..." screen, so the surface is never an unexplained black window.
  Qt chooses Wayland or X11, so the window is native on KDE and GNOME. Guest
  exit 21 means the VM was started without ivshmem; a second `wolfser` reuses
  the running instance.
- **SMP:** `-smp 4` selftest 15 PASS / 0 FAIL.
- **Fog QEMU:** relocation, at-link, fail-closed park and timer cases PASS.
- **Init fix:** Silverd resolves the child **task id** before every grant
  (Scout, Howl, Howler); the old code passed spawn cap ids.
- **Power:** `poweroff`/`reboot`; kernel policy gives the shell `CAP_POWER`
  (spawn handles cannot delegate it). SMC on Qualcomm, HVC on QEMU with
  `virtualization=on`; QEMU's emulated PSCI still does not exit the VM.
- **Phone status:** console over UART on `rain` is not verified (UART
  clock/pins unknown). Earlier hardware forced three fixes: the virtio-blk PCI
  probe is QEMU-only, GENI UART TX fails fast instead of stalling per byte, and
  the newer MMU boot proofs are skipped on fog by default
  (`fog-hw-selftests` opts in). The graphics bring-up evidence is historical.
- **Next steps:** Linux ABI slice 61 — GTK3 still needs the non-contiguous /
  streamed file-backed mapping and a larger per-task mmap window (slice 59's
  blocker); then GTK4 and the Phosh track. Remaining audit lows: LX-8 (fd
  identity is path-based), LX-9 (epoll fd numbers), LX-10 (timerfd
  overflow), LX-11 (writev accounting / console loop), LX-12 (timespec
  rounding), W-6 (ivshmem BAR check), W-7 (outbox backpressure), W-10
  (stale link). Network base maintenance: keep-alive, RTC, TLS 1.2.
  Wolfser is frozen except for bug fixes: applications, including the browser,
  come from Linux — **YouTube/JS/media require a WebKit/Gecko-class engine on
  the Linux ABI + Phosh track**, not another reader increment.
  Parked: QEMU PSCI shutdown, fog TLB proof, GENI UART on `rain`, tablet,
  Redmi storage/power.

## 2026-11-06 — SFS moves toward the kernel

Milestone: the kernel now owns a synchronous disk path and mounts SFS in the
VFS.

- `src/virtio_blk.rs` — polled virtio-blk-pci driver inside the kernel (BAR4 at
  `0x1010_0000`, modern layout common/ISR/device/notify at 0/0x1000/0x2000/
  0x3000, one queue, one outstanding request, `dc cvac`/`dc ivac` fences).
  Boot probe reads sector 0 and prints `KBLK: sector 0 magic 0x53465321`.
  It resets the device afterwards so the EL0 `block_driver` re-initializes it
  cleanly; `sfsinfo` and the userspace write path still work.
- `src/sfs.rs` — read-only SFS backend for the VFS. `sfs::load()` snapshots the
  whole 1 MiB image before the EL0 driver claims the disk, and the backend
  serves `read_dir`/`read_at`/`len` from that snapshot. Mutating operations
  fail closed.
- `src/vfs.rs` — `mount(..., "sfs", ...)` constructs the new backend.
- `src/main.rs` — loads the snapshot after the frame allocator, then mounts it
  at `/sfs` right after `vfs::init`/`seed`.

Verified in QEMU with a disk carrying `a.txt = "hi"`:

```
howl> ls /sfs
a.txt
howl> cat /sfs/a.txt
hi
```

Not done yet: the write path stays in the userspace `sfs_server`, so `/sfs` is
a boot-time snapshot; nested directories and indirect blocks are still
unsupported. Next step is to make the kernel driver the sole owner of the
device (drop `block_driver`/`sfs_server` from the boot chain) and add write
support plus an on-demand (not snapshot) block read path.


Checkpoint: package E2, the external audit fixes (`b22e3a0`), the SMP
hardening (`b2f0e57`, `145ef54`), the quota / DMA-quarantine / TLB-test
package (`681e64c`), the four-core bring-up (`ad20630`), the audit fix
round plus honest CPU accounting (`af23b29`), fail-closed FDT reservations
(`a8e948e`), and FIFO ready queues with a per-CPU stall watchdog
(`35c44b4`): owner-aware page-table walks, explicit DRAM bounds and a
portable cache-line helper; fail-closed FDT/region parsing; zeroed DMA
publication; serialized PL011 RX; GICv3 single-window bound-IRQ dispatch;
AP publication gated on a usable banked GIC; round-qualified quiescence;
up to four QEMU CPUs with per-CPU timer liveness; an FDT-sized CPU slot
count; in-order dispatch; three priority classes with aging; push migration
to idle peers; a kernel-lock stall detector; and a monitor that reports a PE
which stops making progress.

**SMP feature program (2026-11-04):** four packages, in order —
(1) per-CPU ready queues **[done]**, (2) ASID-tagged address spaces **[done]**,
(3) DMA in-flight quiescence contract **[done]**, (4) SMP churn stress
**[done]**. Each package is verified on its own before the next starts. The
churn harness found three scheduler bugs; all are fixed and the harness now
passes repeatedly on `-smp 4`. It stays opt-in so `smp-selftest` does not
depend on four online CPUs.

- **Page-table ownership and DMA reclamation (Packages E/E1/E2).** Every page
  table is registered with `(frame, generation, owner)`; walks validate the
  owner of the root and of every level before touching a descriptor. A
  foreign or unverifiable table fails closed. `revoke_phys_range` accepts only
  `[DRAM_BASE, DRAM_END)`, and DMA frames are freed only when the CPU-mapping
  scan proves `safe_to_free`. `safe_to_free` says nothing about device
  quiescence; see the DMA contract below.
- **GICv3 readback correction (2026-11-03, after E2).** `ca7fd68` added a
  fail-closed `GICR_IGROUPR0` check that is a false negative on the Rain unit:
  the register reads `0` for PPI 18 even though the same PPI is delivered to
  EL1 as Group 1 NS. The gate now requires `GICR_ISENABLER0` plus
  `GICR_IGRPMODR0` and the distributor's `ARE_NS`/`EnableGrp1NS` readback; the
  real timer tick proves delivery. `GICR_IGROUPR0` remains diagnostic only.
- **Post-E2 hardware smoke test.** `fastboot boot` of the E2 `fog-ui` image
  with the readback correction reaches the status panel with `GICV3 ONLINE`,
  `TIMER ONLINE` and growing `TICKS`. This validates the boot path, MMU
  self-test, GIC/timer and scheduler/Howl start; it does **not** stress DMA
  reclamation, TLB shootdown or cache maintenance under load.
- **SMP Phase 1 (opt-in `smp`, QEMU only).** Default builds stay single-core.
  `no_std,smp` with QEMU `-smp 4` brings CPU1–CPU3 online through physical AP
  trampolines (correct PSCI `0xC4000003` HVC call, physical entry/params,
  `CPACR_EL1` FP/SIMD enable, high-half jump), initializes each GIC/timer and
  accepts IPIs **and executes tasks**. Per-CPU idle contexts plus AP idle ids
  above the user id space fixed the shared-idle livelock without shifting
  `SCOUT_ID`; QEMU `-smp 4` reaches `howl ready`, commands work and the
  `demo-tasks` suite is clean, while `-smp 2` still runs with CPU1 only.
  Fog/tablet still force one CPU. Space quiescence for teardown/DMA
  revoke, TLB shootdown under load and physical SMP remain unproven; details
  and the Phase 5 list are in [`SMP_STATUS.md`](SMP_STATUS.md). A stop-the-world
  SGI1 handshake now parks every other CPU (including BKL waiters) around DMA
  teardown, with a `dsb sy` before acknowledgement; a timeout falls back to the
  conservative `space_is_running` / `range_mapped_by` leak gate. The handshake
  is round-qualified: `QUIESCE_ROUND` / `QUIESCE_RELEASE` / a monotonic
  per-CPU `QUIESCE_ACK` replace the old boolean triple, so a CPU parked across
  `quiesce_end` → `quiesce_begin` acknowledges the new round instead of being
  stranded (which used to time out the next round and leak DMA frames). An AP
  is published online only after its banked GIC bring-up succeeds; a failed AP
  parks offline, and late `boot_secondary_cpus` failures release the stack,
  private page and task space instead of leaking them. The opt-in
  `smp-selftest` feature now runs 64 back-to-back SGI1 rounds and checks that
  every acking CPU observed the generation published before each request.
- **Four-core bring-up (2026-11-04).** `cpu_count()` reports `MAX_CPUS` (4) on
  QEMU; `boot_secondary_cpus` probes every slot and keeps going when `CPU_ON`
  rejects one, so `-smp 2`/`-smp 3` leave the missing slots offline instead of
  aborting. `EXPECTED_CPUS` records the accepted set and the self-test waits
  for exactly that many CPUs. Each AP still receives its own `ApBootParams` and
  claims the first free `CPUS` slot, so concurrent CPU2/CPU3 starts cannot
  corrupt each other. Kernel console writes take a bounded best-effort writer
  lock (bytes no longer interleave inside one call), and the timer keeps a
  per-CPU tick counter so `smp-selftest` can prove every online PE takes its
  PPI (`ticks PASS`) even when the SGI path works. Verified: `-smp 4` brings
  CPU1–CPU3 online with `quiesce PASS (64 rounds, 3 peers)` and
  `ticks PASS (3 peers)`; `demo-tasks` on `-smp 4` is 69 PASS / 0 FAIL /
  0 PANIC with no DMA leak.
- **Ctrl-Z regression fixed after S-01.** The audit added a
  `CAP_CONSOLE_GRANT` gate to `SYS_STOP_CONSOLE`, but Howler's initial
  self-capabilities did not include it, so the userspace UART driver could no
  longer stop the console owner. `initial_self_rights` now grants Howler
  `CAP_MMIO | CAP_IRQ | CAP_CONSOLE_GRANT`; the other embedded programs keep
  their existing minimal sets.
- **Redmi 10C / fog RAM-only bring-up is implemented but not hardware-verified.**
  Authoritative document: [`redmi10c_poc/README.md`](redmi10c_poc/README.md).
- The default `fog` build is **fail-closed**: relocation is disabled, and a
  non-link entry records `pc`/`link`/`delta` in `BOOT_PARAMS` and parks. It
  never copies into `0x4008_0000` by default.
- `fog-reloc` is an explicit opt-in. It validates the ARM64 header, image size,
  both source and destination span ends, overflow and every overlap before
  copying, **and** runs a pre-copy live-FDT proof (`src/fog_validate.rs`): the
  destination must be inside `/memory` and outside every reservation,
  memreserve, the DTB and the running source image. It is still not the
  hardware-ready default; unknown cell layouts, non-empty `/reserved-memory`
  `ranges` and malformed trees park.
- GICv3 no longer falls back to redistributor frame 0. The boot CPU's frame is
  matched by `GICR_TYPER` affinity in bits `[63:32]`; if no frame matches, the
  timer PPI is left disabled and timer-dependent scheduling does not start.
- **Memory and DMA hardening (2026-11-04).** Three kernel-side additions:
  - A per-task anonymous-mapping quota (`MAX_MMAP_BYTES_PER_TASK`, 16 MiB)
    enforced before any frame or PTE is touched, so one EL0 task can no longer
    `mmap` the whole machine. The pure arithmetic lives in
    `silver_microkernel::mmap_quota_ok` and has host tests.
  - A DMA quarantine: a range whose revoke scan proved `safe_to_free` is held
    for `DMA_QUARANTINE_EPOCHS` scheduling passes (~80 ms) before its frames
    return to the allocator. This bounds the window in which an armed device
    queue could write into a recycled frame; a full quarantine falls back to
    the leak policy. `run_dma_quarantine_self_test` proves the window at boot.
  - A TTBR0 TLB self-test (`mmu::run_tlb_self_test`): the boot CPU switches to
    a scratch space, reads a value, repoints the same VA at another frame and
    reads again. Removing the `tlbi ...is` from `flush_after_table_change` makes
    this boot test stop with exception 9, so the test is proven sensitive.
- **Batched table publication (2026-11-04, XNU `pmap`-style).** `map_user_pages_at`
  and the new `map_user_heap_pages` clean each touched descriptor line as they
  write it and issue one `dsb ishst` + broadcast `tlbi vmalle1is` + `dsb ish`
  per call instead of a full table clean and shootdown per page. `SYS_BRK`
  growth now allocates its pages first and publishes them as one all-or-nothing
  batch, so a several-hundred-page grow pays one shootdown. The boot TLB test
  exercises the batched path; removing the `tlbi` from `finish_table_changes`
  still fails the boot with exception 9.
- **Cross-PE TLB shootdown proof (2026-11-04).** The boot TLB test proves the
  flush on the executing PE only. The opt-in `smp-selftest` build now also runs
  a cross-PE probe: every AP parks before the scheduler loop
  (`smp::ap_tlb_probe_wait`), switches to a scratch space built by the boot CPU,
  reads the probe VA and acknowledges; the boot CPU then re-points the same PTE
  with the normal batched publication and releases the peers, which read the VA
  again and must observe the new frame. With the broadcast `tlbi vmalle1is`
  replaced by a local `tlbi vmalle1`, the probe reports `tlb cross-PE FAIL
  (3 CPU(s) kept the stale translation)` while quiesce/ticks still pass — the
  test is sensitive to a non-broadcast flush. The peers run with IRQs masked
  and no kernel lock, so the boot CPU can drive them while holding the BKL
  without a lock-ordering deadlock.
- **Scheduler invariant audit under `smp-selftest` (2026-11-04).** Every
  scheduling pass, after `reap_dead_tasks` (the one place that shifts task
  indices), `verify_scheduling_invariants()` checks the cross-CPU pairing:
  each online CPU's `current_index` either is `usize::MAX` or names a task whose
  `running_on` is that same CPU; every `running_on = Some(cpu)` names an online
  CPU whose current index is that task; a `Running` task names a CPU; no two
  CPUs name the same task. A violation prints the exact reason and panics
  instead of letting a task resume with another task's address space. The
  check is a no-op outside `smp-selftest`.
- **Tick-liveness selftest window (2026-11-04).** The timer check used to run
  while the boot CPU held the BKL. With real tasks present (the new
  `demo-tasks,smp-selftest` profile) an AP that entered `schedule` from a timer
  IRQ spins in `bkl_acquire` with IRQs masked, so it could not take another
  PPI and a healthy system reported `ticks FAIL`. The observation window now
  releases the BKL (the boot task stays `Running`, so no other CPU can claim
  it) and reacquires it afterwards.
- **Per-CPU ready queues (2026-11-04).** `TaskManager.ready` holds one FIFO per
  CPU of `(task id, index)` pairs for Ready, non-idle tasks.
  `find_next_task` tries the local queue, then steals one task from a peer,
  then falls back to the previous full scan (which still owns idle-task
  selection and covers any missed enqueue). Because entries carry the id, an
  index shift can only discard an entry, never dispatch the wrong task; the
  removal paths (`take_zombie`, zombie-cap `retain`) rebuild every queue. The
  `smp-selftest` invariant audit also validates queue ids/indices, and a
  one-shot counter line reports the dispatch mix. The combined profile shows
  the fast path working (`ready queues local=.. steal=.. scan=..`); a debug
  build that corrupts a queued id fires `sched invariant FAIL (a ready queue
  entry names the wrong task)`.
  **FIFO correction (2026-11-05, `35c44b4`):** the queue was a `Vec` with
  `push` + `pop`, i.e. a stack. With three or more runnable tasks the oldest
  entry was never dispatched (`A,B,C` ran `C,B,C,B,...` and `A` starved until
  it blocked). It is now a `VecDeque` with `push_back`/`pop_front`;
  `run_ready_queue_selftest` pops through the real `dequeue_ready`, requires
  spawn order, and fails with `positions=2,1,0` when the queue is switched
  back to `pop_back`. The first version of that test drained the deque
  directly and would not have caught the bug; the negative run exposed it.
- **ASID-tagged address spaces (2026-11-04).** `create_task_space` allocates a
  unique ASID (`1..=255`; `TCR_EL1.AS` stays 0, the ARMv8.0 8-bit baseline) and
  tags the returned TTBR0 in bits `[63:48]`. `switch_address_space` now only
  writes TTBR0 and executes `isb`: the outgoing space's translations stay
  valid and tagged, so a context switch no longer broadcasts
  `tlbi vmalle1is` to every CPU. A PTE edit invalidates only the edited space
  with `tlbi aside1is` (still inner-shareable, so peers drop the stale entry);
  `destroy_task_space` invalidates the ASID before retiring the tables and
  returns it to a live-bitmap pool. Every function that treats a `ttbr0`
  argument as a frame address goes through `root_of()`; the boot/kernel TTBR0
  stays ASID 0. A new boot test maps the same VA in two spaces to different
  frames and switches between them **without any flush**, requiring each read
  to return its own value; dropping the ASID tag makes it fail closed with
  exception 9.
- **In-flight DMA lease contract (2026-11-04).** `DmaLeaseTable` (pure,
  host-tested) holds up to 16 physical `(start, length)` claims. An in-kernel
  driver arms a `DmaLease` before handing a buffer to a device and drops it
  only after observing completion and stopping the queue. The revoke path now
  checks `dma_range_in_flight()` before touching the range: a live lease means
  the device may still write, so the frames are leaked instead of being
  quarantined and reused. This is a stricter gate than the time window, which
  can only bound a quiet range. The boot self-test proves the overlap rules at
  both range edges and after drop; disabling the gate fails the boot with
  exception 9. Userspace drivers cannot arm a lease yet — that needs a syscall —
  so their ranges still rely on the quarantine window alone; the gap is
  explicit in `map_dma_to_user` and `SMP_STATUS.md`.
- **Global monotonic clock (2026-11-04).** `timer::ticks()` no longer counts
  only the boot CPU's timer interrupt. It derives 100 Hz ticks from
  `CNTPCT_EL0` relative to the base captured in `timer::init`, so one clock
  serves every PE and deadlines survive a CPU that cannot take its PPI. The
  per-CPU counters stay as liveness evidence for `ticks PASS`. A boot test
  masks the boot CPU, waits for one tick and logs
  `clock PASS (CPU0 masked, ticks +N)`; the same test fails when `ticks()`
  is switched back to the boot-CPU-only counter. This closes the churn-time
  stall where six sleeping tasks never reached their deadlines.
- **SMP churn harness (2026-11-04, opt-in `smp-churn`).** The boot CPU spawns
  12 short-lived kernel tasks with explicit high ids (`next_id` is saved and
  restored around each spawn so user spawn ids stay in lockstep with per-task
  cap-handle ids), releases the BKL, lets the APs dispatch/yield/sleep/exit
  them, then reaps with the invariant audit active. It found three real bugs,
  all fixed:
  - a scheduling pass was preemptible, so a timer IRQ could run a second pass
    on a half-applied `current_index` snapshot; when the outer pass resumed it
    republished the stale index. `schedule` now masks IRQs for the whole pass
    and refuses a nested entry (`SCHED_PASS_ACTIVE`).
  - kernel-task `yield`/`sleep` release the kernel lock through
    `schedule_released`, but a kernel entry drops its inherited hold first, so
    the release had no matching acquire and wrapped `bkl_depth` to
    `u32::MAX`. Both now go through `schedule_step`, which acquires the lock
    when the caller holds none.
  - an IRQ taken between the CAS that publishes the lock owner and the
    `*depth = 1` store saw `owner == this CPU` with depth 0 and could re-enter
    `schedule`, whose acquire can never succeed. `bkl_acquire` now masks IRQs
    across the acquisition, and `bkl_release` fails safe (reports once, does
    not decrement) if a release has no matching acquire.
  A dead task also no longer parks holding the lock when `schedule` finds no
  next task:
  `exit_current` drains any remaining depth before its `wfi` loop. Kernel
  tasks no longer print an `EXIT:` line (`program == u64::MAX`). The harness
  is stable with the audit active and, since the online-count policy landed,
  passes on `-smp 2` as well as `-smp 3`/`-smp 4`. It stays opt-in because it
  is a stress/injection suite (it also runs the masked-IRQ watchdog probe),
  not because of a CPU-count assumption.
- **Audit fix round and honest CPU accounting (2026-11-04, `af23b29`).**
  Six of seven audit findings were already closed in `b22e3a0`; the round
  verified each one and added the missing evidence, plus one real fix.
  `F1`: the per-CPU index snapshot was a compact `Vec` of online CPUs zipped
  in the remap, so an AP publishing `online` between snapshot and remap
  shifted every later entry; the snapshot is now keyed by CPU id and a `None`
  entry (no current task) is skipped. `F2`: the heap L3 descriptor kind is
  validated before dereference, and a boot test empties the heap through the
  generic unmap and maps again. `F3`: `map_dma_to_user_space` lets a boot test
  poison a recycled frame and verify the published frame is zeroed. `F5`:
  a wraparound/full-drop RX-ring test. `F6`: a policy test proves the console
  owner holds `CAP_CONSOLE_GRANT` and a plain driver does not. `F7` is below.
  `F4` was verified in the tablet disassembly. CPU accounting: `Fdt::cpu_count`
  sizes the slot count from the live tree, `smp::online_cpu_count` is used by
  policies that must reflect the machine that actually came up, and the churn
  rule now passes on `-smp 2` as well.
- **Reservation parsing fails closed (2026-11-04, `a8e948e`).**
  `memreserve_regions` had already returned `Option`; the `/reserved-memory`
  child path still turned a malformed or overflowing `reg` into `0`. Both
  `reserved_regions` and `memreserve_regions` now return `Option<usize>` and
  `fog::probe` marks the tree truncated on `None`, so the planner never hands
  out firmware-reserved memory. Host tests cover an overflowing child `reg`, a
  carve-out list that does not fit, and an oversized memreserve map.
  `CpuCounter` also counts only direct `/cpus` children.
- **Priority classes with aging (2026-11-05).** The ready queue now carries a
  scheduling class per entry (0 high, 1 normal, 2 low; every task defaults to
  Normal) and an age in dispatch attempts. `dequeue_ready` ages every queued
  entry once per call and dispatches the FIFO-first entry with the best
  effective class; `sched_policy::effective` promotes one class every
  `AGING_PASSES` (8) rounds, so a low-class task reaches the top class after
  at most `(classes - 1) * AGING_PASSES` attempts regardless of how many
  high-class arrivals keep the queue busy. Within a class the order stays
  FIFO. The decision function is host-tested (`sched_policy`, 6 tests); the
  boot probe `run_priority_selftest` drives the real queue: high enqueued last
  is dispatched first, normal probes keep spawn order, and a low probe under
  constant high pressure wins after 16 rounds
  (`priority PASS (high first, FIFO in class, low aged in 16 round(s))`).
  Setting `AGING_PASSES` to 1000, i.e. disabling aging, produces
  `priority FAIL (... aged_round=0)` while the order part still passes.
  There is no syscall: `set_task_priority` is kernel-only and userspace tasks
  stay Normal, so the EL0 ABI is unchanged.
- **Push migration (2026-11-05).** After a successful local dispatch, a CPU
  that still has queued work calls `push_migrate`: `cpu_is_idle` builds an
  idle view, `sched_policy::push_target` picks the first idle peer with an
  empty queue, and the back entry is moved there with its `age` intact, so a
  migrated low-class task does not restart its aging. A push is refused when
  the destination already has local work, which would reorder its queue for
  no gain. The policy is host-tested; the dispatch mix now reports `push=`.
  A live AP is normally parked on the kernel lock during the boot selftests
  (no current task or a stale index), so `run_push_selftest` passes an
  explicit idle mask to `push_migrate_with` and verifies the real queue move;
  the live `cpu_is_idle` path is evidenced by `push=1` in ordinary `-smp 4`
  runs. Disabling `push_target` makes the probe fail (`moved=0`, `push=0`)
  while churn still passes. No IPI is sent: an idle PE re-evaluates its queue
  on the next 10 ms timer tick.
- **Partial `munmap` (2026-11-05).** `Task::munmap` accepts any page-aligned
  sub-range of one anonymous mapping, not only an exact region match. The
  host-tested `mmap_remove` locates the containing region and returns how many
  pages stay on each side; the task unmaps the range, frees only that slice of
  the original contiguous run (every page shares the run generation), trims the
  left part and splits off the right part when the range is in the middle. The
  removed pages are released from the ledger, and the resulting hole is
  reusable by the next first-fit `mmap`. A split needs one extra bookkeeping
  slot, so a full `mmap_regions` table refuses the request before touching a
  PTE. `run_mmap_split_selftest` maps four pages, removes the middle two,
  proves the ends survive and the hole is gone, refills it, and releases
  everything back to the ledger baseline (`MMAP SPLIT PASS (regions=2,
  ledger=14)`). Making every `munmap` exact-match again makes the boot proof
  fail and panic.
- **Per-task memory ledger (2026-11-05).** Each task carries `mem_frames`,
  the frames it can pin: the fixed address-space tables and kernel/user stacks,
  the private page, the ELF image, `brk` pages, anonymous `mmap` (data plus an
  upper-bound estimate of the L3 tables it creates) and DMA runs. The limit is
  `mem_policy::task_frame_limit(total)` — a quarter of the usable window, with
  a small floor so a tiny machine still spawns — and every charge is
  all-or-nothing with a matching release on shrink/unmap/failure. The ledger
  complements the global reserve: the reserve bounds the machine, the ledger
  bounds one task. `run_task_ledger_selftest` charges two frames against a
  forced limit, requires the next frame to be refused without changing the
  counter, releases and reopens (`TASK LEDGER PASS (limit=... frames,
  before=...)`); disabling the check gives `TASK LEDGER FAIL` and a panic at
  boot.
- **User-memory reserve (2026-11-05).** User-visible allocations no longer go
  straight to the frame allocator: `alloc_user_page_tagged` and
  `alloc_user_contiguous_pages_tagged` refuse a request once granting it would
  leave fewer than `mem_policy::reserve_frames(total)` frames free (one eighth
  of the usable window, capped at half so a tiny fog window can still start a
  task). The policy is a pure host-tested function of the current free count,
  so no per-allocation accounting can drift; every user path runs under the
  kernel lock, which serializes the check with the other CPUs' requests. The
  gated paths are `brk`, anonymous `mmap`, DMA allocation, ELF image pages,
  and the per-task user stack/private page; kernel metadata (page tables,
  kernel stacks, AP stacks, boot self-tests) keeps using the raw allocator.
  `run_user_reserve_selftest` forces the reserve one frame above the free
  count: the user allocator must refuse while the raw allocator still works,
  and clearing the override must reopen the gate (`MEM RESERVE PASS
  (free=... reserve=...)`). Disabling the gate gives `MEM RESERVE FAIL
  (... refused=0)` and halts the boot.
- **Data/instruction caches (2026-11-05).** The boot stub builds the tables
  and enables the MMU with `SCTLR_EL1.C = 0`, `I = 0`, so every access is plain
  memory. `mmu::init` now ends in `cache::init_boot`, which invalidates the
  inherited data cache by set/way (boot-only, single PE, so no shared-cache
  hazard yet), invalidates the instruction cache, and sets `C | I` in one
  `SCTLR_EL1` write. Every AP calls `cache::enable_secondary` as its first
  Rust-side action, before console/slot/scheduler state, so its memory
  attributes match the boot CPU's before it touches shared data; it does not
  run the set/way sweep, which would be unsafe once another PE has dirty
  lines. Each CPU stores the `SCTLR_EL1` value it read back and
  `run_cache_selftest` requires `C=1 I=1` for every online CPU
  (`SMP SELFTEST: cache PASS (4 CPU(s) C=1 I=1)`); a build with
  `cache::ENABLE_CACHES = false` boots with `CACHE=OFF` and reports
  `cache FAIL (... local_sctlr=0x...50839)` rather than skipping silently.
  Two consequences that only matter once caches are real:
  `boot_secondary_cpus` now cleans the freshly zeroed AP stack to the point of
  coherency (a dirty zero line in the boot CPU's cache would otherwise be
  written back over the AP's live stack), and `run_dma_zero_self_test` now
  crosses the cacheable and Normal Non-Cacheable aliases in both directions,
  cleaning before the device reads and invalidating before the CPU reads after
  the device wrote. QEMU does not model caches, so that probe is HIL evidence;
  the `SCTLR_EL1` read-back is what the emulated run proves.
- **Kernel-lock stall detector (2026-11-05).** `bkl_acquire` now timestamps
  the start of its retry loop and, after one second without acquiring (or 50M
  yields before global time exists), prints one `BKL STALL:` line per waiter
  episode with the holder CPU, holder depth, waiter CPU/depth, elapsed ticks,
  spins, and the holder's task id. Each CPU mirrors its current task id into
  an atomic (`PerCpu::current_task`) when the scheduler changes its index, so
  the diagnostic reads a plain atomic on a PE that does not hold the lock
  instead of walking the scheduler vector while another CPU mutates it;
  `bkl_depth` is likewise an atomic read-only-from-peers field. Printing is
  single-flight (`BKL_REPORTING`) so two waiters never interleave their fields
  into one unreadable console line; the counter still counts every episode.
  capped at one report: the holder may recover. There is no recovery
  mechanism — stealing a live lock is unsafe — and the report is a diagnostic
  for a bug that would otherwise look like a silent hang. Quiesce parking
  restarts the window, because a stop-the-world round intentionally blocks
  waiters. `run_bkl_stall_selftest` holds the lock just past the threshold
  while the APs are blocked in `bkl_acquire` and requires their reports; the
  reports are silenced for the probe so ordinary logs stay clean. Disabling
  the detector makes the probe fail (`bkl stall FAIL (reports=0)`).
  The detector immediately found a real bug in our own `stall_probe_task`: a
  newly dispatched kernel task inherits the dispatcher's BKL hold, and the
  probe did not drop it, so its 1.2 s masked spin also blocked every other
  CPU. It now drops the inherited hold exactly like `churn_task`.
- **Per-CPU stall watchdog (2026-11-05, `35c44b4`).** A PE that has entered
  the scheduler loop must take its timer PPI or complete a scheduling pass
  every check round. After three consecutive 250 ms rounds without progress,
  any other PE prints `WATCHDOG: CPU<n> stalled (beats=... index=... rounds=3)`
  once per episode; it never panics, because a long IRQ mask can recover.
  The decision is the host-tested `silver_microkernel::watchdog_policy`
  module. Exemptions keep the signal honest: only PEs that called
  `cpu_started` are judged (AP bring-up and `ap_tlb_probe_wait` are out), a
  stop-the-world quiesce rebases every grace window, and a CPU parked in
  `bkl_acquire` is marked waiting because the dedicated BKL stall detector
  owns that condition.
  The boot logs `WATCHDOG: policy PASS` and
  `WATCHDOG: heartbeat PASS (3 peer CPU(s) ticking)`. The heartbeat check
  deliberately excludes the CPU running it: that CPU is inside a pass with
  IRQs masked and may not have taken a tick yet, which used to make the test a
  boot-timing race (`live=3/4`); its PPI path is covered by the clock/ticks
  tests and the post-boot monitor. The opt-in `smp-churn` profile adds a real
  negative test: a kernel task masks IRQs and spins for 1.2 s, another PE
  reports it, and the task is retired (`watchdog stall PASS`). Disabling the
  check call yields `watchdog stall FAIL (reported=0 ...)`, so the harness is
  not vacuous. The opt-in churn profile still has a known intermittent
  `sched invariant FAIL (a CPU's current task is not marked running on it)`
  after the stall probe retires (observed roughly once in seven runs before
  the boot-proof ordering fix, zero in 21 runs after it); it is timing
  sensitive and not root-caused, so churn remains diagnostic only.
- Verified in this snapshot: **182 host tests**, fog QEMU relocation/at-link/
  fail-closed-park/reserved-destination cases, normal QEMU boot to `howl>`,
  demo regression **69 PASS / 0 PANIC** on both the default build and
  `no_std,smp` with QEMU `-smp 2` and `-smp 4`, `no_std,smp-selftest` logging
  `ready fifo PASS (3 probes in spawn order)`,
  `priority PASS (high first, FIFO in class, low aged in 16 round(s))`,
  `WATCHDOG: policy PASS`, `WATCHDOG: heartbeat PASS (3 peer CPU(s) ticking)`,
  `push PASS (probe moved to CPU<n>)`,
  `bkl stall PASS (3 waiter report(s))`,
  `SMP SELFTEST: cache PASS (4 CPU(s) C=1 I=1)` and `CACHE=ON line=64`,
  `tlb cross-PE PASS (3 peer CPU(s) saw the remap)`,
  `quiesce PASS (64 rounds, 3 peer CPU(s))` and `ticks PASS (3 peer CPU(s))`
  on `-smp 4` (one peer on `-smp 2`), and the combined
  `no_std,smp,demo-tasks,smp-selftest` profile at **80 PASS / 0 FAIL / 0 PANIC**
  with zero invariant failures and a `ready queues local=.. steal=.. scan=..
  push=..` dispatch-mix line, including `clock PASS (CPU0 masked, ticks +1)`. The
  opt-in `no_std,smp-selftest,smp-churn` profile passes `churn PASS` and
  `watchdog stall PASS` on `-smp 2`, `-smp 3` and `-smp 4` with the audit
  active and no lock underflow or real BKL stall report; the stall is detected on
  different APs across runs (CPU1 and CPU3 observed). A negative build that
  drops the `running_on` claim makes the invariant check fire on the next pass
  (`sched invariant FAIL (a CPU's current task is not marked running on it)`).
  Tablet build, and fog boot image v3. The boot self-tests log
  `MMU TLB OK`, `MMU ASID OK`, `MMU HEAP OK`, `DMA ZERO OK`, `MEM RESERVE OK`,
  `TASK LEDGER OK`, `MMAP SPLIT OK`,
  `UART RING OK`, `CONSOLE CAP OK`, `DMA QUARANTINE OK` and `DMA LEASE OK`; a
  build that drops the ASID tag fails the ASID test with exception 9 before
  the scheduler starts, and a build with the lease gate disabled fails the
  lease test the same way.
- **Hardware coverage after E2 is a smoke test, not a proof.** The post-E2
  image boots on physical `rain` and the GIC/timer panel is live, but QEMU and
  a single boot are not evidence for TLB/cache maintenance under load or for
  device quiescence during DMA reclamation.
- **Hardware readiness: NOT READY — NEEDS DEVICE DATA.** The ABL load address,
  live memory/reserved map, UART clock/pins, GIC/timer wiring and boot CPU
  affinity of the specific unit are still unconfirmed.
- A connected unit was identified as Redmi 10C NFC **`rain`** (`220333QNY`),
  same `KHAJE`/SM6225 platform but not the non-NFC `fog` codename. Its
  bootloader is locked (`ro.boot.flash.locked=1`), so no RAM-only boot was
  attempted. ADB without root cannot read `/proc/iomem` or FDT properties.
- The obsolete `tablet/` tree (including `tablet/HANDOFF.md`) was removed in
  TASK №20.2. References to it below are historical; the current hardware port
  is `fog`.

## SMP PROGRAM (TASK №6/№7) — in progress, checkpointed 2026-11-01

Authoritative status lives in **`SMP_STATUS.md`** (read it first). Summary:

- **Checkpoint commit `5f1f888`** ("wip(smp): checkpoint scheduler and IPC
  investigation") holds the whole work delta after `ca84fc9`: TASK №6 phases
  1–6 (new `src/smp.rs`: per-CPU area via TPIDR_EL1, SpinLock, reentrant BKL,
  IPI helpers; per-CPU `current_index`; frame allocator / page-table registry
  under spinlocks; GIC init/cpu_init split; per-CPU timer; TLB shootdown via
  `vmalle1is + dsb ish`) plus TASK №7 fixes (bounds-checked `running_on`
  clear, `&mut`-aliasing removed from `schedule()`, restored
  `bkl_release_asm` in `silver_user_bootstrap`, post-switch `bkl_release()`,
  defined `unregister_mutex` not-found path, ipc receive bisect bypass
  removed).
- **Verified working at that checkpoint:** single-CPU boot unchanged;
  `cargo test --lib` 13/13 (the suite has since grown; see CURRENT STATUS);
  debug and release builds pass.
- **Historical (fixed): `-smp 2`** died at silverd's first blocking IPC
  receive at the 2026-11-01 checkpoint; the layers below are the investigation
  record, not the current state (see the status section at the top).
  Two independent failure layers, both new relative to pre-SMP:
  - Layer A: O3+LTO emits literal `b .` self-loops on *reachable* paths
    (~22 sites: raw_lock/raw_unlock, Semaphore::wait, Channel send/receive,
    ipc send/try_send, sys_receive, drop glues…). The pre-SMP binary built
    with the same toolchain has **zero**. Root cause NOT established;
    `opt-level=1` builds have no holes. Bisect-excluded: BKL, IPI,
    `#[inline(never)]`, tail-call elim, LTO off, debug-assertions.
  - Layer B (probable primary): with a hole-free O1 build the kernel hits an
    **infinite Prefetch Abort** (ESR `0x86000006`, FAR `0x40081a00`): the
    exception-vector page itself becomes unexecutable at runtime — kernel
    translation damage whose writer is still unidentified. This likely also
    explains the "impossible" values (e.g. `1` in `owned_mutexes`) seen in
    earlier gdb captures; that observation must NOT be treated as proven
    memory corruption.
- **CPU1 never comes online** (PSCI `CPU_ON` returns `-1`) and AP boot is
  bisect-disabled; SMP is NOT verified end-to-end.
- **Next steps (priority):** (1) catch the writer of frame `0x40081000`
  (gdb hw-watchpoint on the TTBR1 L2 descriptor / physical frame);
  (2) re-audit `git diff ca84fc9 -- src/mmu.rs` against TASK №5 semantics;
  (3) if layer B is fixed, re-check whether layer A disappears;
  (4) then: PSCI conduit/CPU_ON, AP boot re-enable, bisect-cruft cleanup
  (list in SMP_STATUS.md), full QEMU regression -smp 1/2.
- Diagnostic serial prints (M1–M5, S1/S2, `SYS <n> sp=`, C1–C3) and RAM
  probes (`SMP_PROBE`/`TICK_PROBE`/`DIAG_SNAP`) are kept **on purpose**
  until layer B is understood.

## TABLET PORT (Lenovo Legion Y700, SM8650) — historical, tree removed

> **Superseded (2026-11-03):** the `tablet/` directory, including
> `tablet/HANDOFF.md`, was removed in TASK №20.2. The current hardware
> bring-up target is the Redmi 10C (`fog`); see `redmi10c_poc/README.md`.
> The text below records the earlier tablet investigation and is kept for
> history only.

High-level status as of 2026-10-30:

- **Breakthrough (historical)**: the (now removed) EFI/PE stub executed on the
  device via GRUB `chainloader`. First visible proof: the stub painted the
  splash framebuffer (`0xD5100000`) and a GREEN band appeared.
- State when the tree was removed: the stub reached entry + kernel copy
  (green+white bands) but hung at its own MMU-disable step.
- Diagnostics on that device: pmsg/ramoops readback was dead; the only channel
  was the GOP framebuffer. QEMU regression was clean throughout.

## State as of 2026-10-29 (visible GUI clicks; "EL0" terminology)

Two polish items landed on top of the existing graphics/input work:

- **Button clicks are now visible in the console.** `gpu_driver` no longer
  relies on its own `syscall::print` (which the kernel drops because the driver
  does not own the console). On a button release it sends an IPC message to the
  shell, which owns the console and prints `GUI: Button '<name>' pressed!`.
  Howl registers itself as `"howl"` at startup; `gpu_driver` resolves that name
  lazily at the first click, so spawn order does not matter. The "Power" button
  still also calls `syscall::power_off()`. The message uses IPC opcode `2`
  (`GUI_BUTTON_PRESSED`), carrying the button label; `0`/`1` are already taken
  by Howler input and pointer samples respectively.
- **Terminology.** ARM64 has no "protection rings": user code runs at EL0 and
  the kernel at EL1. "Ring 3" was replaced with "EL0" (or "EL0 (userspace)")
  project-wide — README, handoff and source comments. Function/variable names
  had no "ring" to rename; the virtqueue "used/available/descriptor ring" names
  are the unrelated circular-buffer meaning and were left alone.

## State as of 2026-10-28 (power-off via PSCI; the GPU "Power" button shuts the machine down)

**Power-off is complete and verified in QEMU.** Clicking the "Power" button on
the engineer menu shuts the virtual machine down through PSCI `SYSTEM_OFF`.

### What changed

- `SYS_POWER_OFF = 71` (`sys_power_off`): issues PSCI 0.2 `SYSTEM_OFF`
  (`0x8400_0008`) with `x1..x3 = 0` via **`hvc #0`**. QEMU's `virt` machine
  boots the guest at EL2 and exposes PSCI through the **HVC** conduit, so an
  `smc` would trap as an EL1 sync exception and halt (`EL1 EXCEPTION kind=0`);
  `hvc` reaches the hypervisor's PSCI handler, QEMU prints "Power down" and
  exits. The call never returns. (The user's original spec said `smc #0`; HVC is
  the correct conduit for this machine.)
- `userspace/src/bin/gpu_driver.rs`: the click handler reports every release to
  the shell over IPC (see the 2026-10-29 section) and calls `syscall::power_off()`
  when the released button is "Power" (index 4 in `BUTTON_LABELS`). The kernel's
  `POWER OFF: PSCI SYSTEM_OFF` line is a raw PL011 write and always visible.

### Verification

Headless: QMP `input-send-event` moves the tablet pointer to the Power button
center (raw `(16409, 17512)` → pixel `(320, 256)`) and clicks. The serial shows
`POWER OFF: PSCI SYSTEM_OFF` and QEMU exits (no `PSCI returned`, no
`EL1 EXCEPTION`).

### Note for later

`SYS_POWER_OFF` is currently ungated: any task can request it and halt the whole
system. It is intentionally narrow (PSCI `SYSTEM_OFF` only — no `SYSTEM_RESET`,
no arguments), but a capability bit or a console-owner check is the natural
hardening once multi-user isolation matters.

## State as of 2026-10-28 (Phase 7 complete: EL0 fault containment + task teardown)

**Phase 7 (EL0 fault handling + teardown) is complete and verified in QEMU.** An
EL0 task — embedded or dynamically loaded — that faults (bad pointer,
privileged MSR, EL1-only MMIO) is killed cleanly and the system keeps running.

```text
howl> run bin/crash
crash: writing to 0x0...
Task 11 killed: Page Fault at 0x0000000000000000 (ESR: 0x0000000092000045)
howl> ps        # task 11 is gone; silverd/scout/howl/howler still listed
```

### What changed

- The sync-exception vector already split EL0/EL1 (`SPSR.M[3:2] == 0b00` →
  `silver_handle_task_fault`); this phase tightened the two handlers rather than
  rebuilding the teardown, which was already correct for dynamic tasks.
- `silver_handle_task_fault` now logs `Task <id> killed: Page Fault at 0x...
  (ESR: 0x...)` (task ID from `scheduler::current_task_id()`, plus `ELR_EL1` and
  the switch trace), then calls `scheduler::exit_current()`. Teardown marks the
  task Dead, closes its IPC/VFS, recovers the console and schedules away;
  `reap_dead_tasks` then releases the kernel and user stacks, the TTBR0 page
  tables and `image.release()` (the ELF frames) — the same path embedded and
  dynamic tasks share, so a crashed `run` program frees exactly its own pages.
- `silver_handle_exception` (the EL1 path) now **halts** in a `wfe` loop after
  dumping kind/ESR/ELR/FAR, instead of `exit_current()`. An EL1 exception is a
  kernel bug that cannot be survived; boot/allocator/IRQ failures reported
  through kind 4..8 use the same halt. This closes the long-standing "EL1 path
  is a stub" tech-debt item.
- `userspace/src/bin/crash.rs` writes to `0x0` via `write_volatile` (so the
  store survives release optimization) and is seeded at `/bin/crash` like
  `hello.elf`. `run bin/crash` faults with a data abort from EL0
  (`ESR 0x92000045`: EC 0x24, DFSC 0x05 translation fault), the kernel kills
  the task and the shell returns to the prompt.

### Verification

`run bin/crash` → `Task 11 killed: Page Fault at 0x0...` → `howl>` returns; `ps`
no longer lists task 11 and `run bin/hello` still prints `Hello from ELF!`. The
embedded boot chain is unchanged.

## State as of 2026-10-28 (Phase 6 complete: dynamic ELF loader)

**Phase 6 (Dynamic ELF loading) is complete and verified in QEMU.** The kernel
now loads an ELF64 file from the RAM-VFS into a fresh EL0 task, without the
binary being embedded at build time. The shell command `run <path>` reads the
file by path, the kernel parses and loads it, and the new task runs in EL0 with
its own address space, stack and W^X page permissions.

```text
howl> run bin/hello
Spawning ELF at 0x0000000040000000 with stack at 0x000000007FE00000
Hello from ELF!
```

### What changed

- `src/elf.rs` is the loader proper. `Elf64Ehdr`/`Elf64Phdr` parse the header and
  program table; `load_elf(bytes) -> Result<LoadedImage, ElfError>` allocates
  contiguous frames per `PT_LOAD` segment, copies file bytes, zero-fills BSS and
  returns an owned `LoadedImage { physical_start, page_count, entry, kinds }`.
  `PageKind` classifies every page as Text (R-X), ReadOnly (R--) or Data (RW),
  and `load_elf` enforces W^X by rejecting writable-executable and unreadable
  segments, overlapping permissions on one page, holes in the image and an entry
  point outside a text segment. `error_name` maps each `ElfError` to a short log
  label. `USER_IMAGE_VA = 0x4000_0000` and the 2 MB image window live here.
- `elf_loader.rs` is now a thin shim: `load(bytes)` delegates to `elf::load_elf`
  and logs the reason on rejection. There are no more parallel `UserImage` /
  `PageKind` types — `elf::LoadedImage` is the single image type everywhere.
- `mmu::create_task_space` and `scheduler::Task.user_image` take
  `elf::LoadedImage`; `Task::new` no longer loads ELF itself. The embedded path
  resolves the image in `spawn_inner` (before any frame is allocated, so a
  rejection needs no cleanup), and the dynamic path passes a pre-loaded image to
  `spawn_elf`.
- `spawn_elf(image, argument) -> Option<CapId>` mirrors `spawn_user_with_cap`:
  it allocates a kernel stack, the user stack and a private page, builds TTBR0,
  sets the entry from `image.entry`, pushes the task READY and returns a
  capability handle so the parent can lend the console, wait and kill. The new
  task receives **no** kernel-granted capabilities — device rights must be
  delegated by its parent. `spawn_elf_task` emits
  `Spawning ELF at 0x... with stack at 0x...` over the kernel UART.
- New syscall `SYS_SPAWN_PATH = 70` (`sys_spawn_path`): the kernel reads the file
  by path from the VFS (`vfs::read_file`), runs `elf::load_elf` and calls
  `spawn_elf`. The kernel reads the file itself because the ELF (tens of KiB) far
  exceeds the 4 KiB single-copy limit; the path is a small checked copyin.
  `EXEC_FILE_LIMIT` (4 MiB) bounds the blob before loading.
- `run <path>` in Howl (`run_elf`) calls `spawn_path`, lends the child the console
  via `console_set`, waits on `wait_event`, and reports exit/stop status exactly
  like `run_utility`; Ctrl-Z (Howler `stop_console`) returns control to the shell.
- `src/seed.rs` seeds `/bin/hello` into the VFS at boot (initramfs-style) from
  `include_bytes!(OUT_DIR/hello.elf)`. `build.rs` builds `hello.elf` alongside the
  other binaries but keeps it **out** of `user_program::image`, so the only way to
  run it is the dynamic loader. `seed::seed()` runs right after `vfs::init()`.
- `PROGRAM_DYNAMIC = u64::MAX - 1` is the sentinel program ID for dynamically
  loaded tasks (`ps` shows them as `elf`), distinct from `u64::MAX` (kernel tasks).

### Design decisions to preserve

- The image type is `elf::LoadedImage`, not a tuple: the scheduler needs
  `physical_start` and `page_count` to install TTBR0_EL1 and later free the
  frames.
- The user stack is reused as-is: `0x7FE0_0000`, 2 pages (8 KB). The image window
  stays `0x40000000..0x40200000` (2 MB). The dynamic loader introduces no second
  stack or image layout.
- Dynamic tasks get an empty capability table by default (secure), unlike
  embedded programs which receive self capabilities at spawn.

### Verification

`run bin/hello` from the `howl>` prompt prints `Spawning ELF at 0x0000000040000000
with stack at 0x000000007FE00000` and then `Hello from ELF!`. The embedded boot
chain is unchanged: all drivers and the shell start normally, and `ps` lists the
dynamic task as `elf`. Build is clean except the pre-existing `capability.rs`
warning.

### Limitations recorded for the next phase

- `run` reads the whole file into kernel memory before parsing it (bounded by
  `EXEC_FILE_LIMIT`), rather than streaming segments. Only static `ET_EXEC`
  images are supported; there is no PIE, dynamic linking, relocation or shared
  libraries.
- The dynamic task's argument is always `0`; there is no argv beyond the fixed
  numeric argument. `/bin/hello` and the `/sfs/apps` bundles are seeded at boot
  by `src/seed.rs`; the SFS image itself is loaded into kernel RAM by the
  in-kernel virtio-blk path, not by a userspace server.
- The user stack is fixed at 2 pages and the image at 2 MB, matching the embedded
  path; neither is configurable per program.

## State as of 2026-10-28 (Phase 5 complete: ramfb graphics + virtio-tablet input, live engineer menu)

**Phase 5 (Graphics + Input) is complete and verified in QEMU.** `gpu_driver`
paints a static "engineer menu" (BIOS / iPhone Field-Test style) onto a `ramfb`
framebuffer, and `input_driver` turns the `virtio-tablet-pci` mouse into a live
cursor that presses and releases the menu buttons.

Why `ramfb`, not PL111: the `virt` machine has no PL111 (that PrimeCell CLCD
exists only on realview/versatile/vexpress boards). `ramfb` is QEMU's native
plain-RAM framebuffer: the guest allocates a contiguous frame buffer, writes a
28-byte `RAMFBCfg` descriptor to the `etc/ramfb` `fw_cfg` file, and then paints
pixels; QEMU's display surface scans the RAM out.

### Current boot chain

`silverd` starts, in order:

1. Scout (bounded `SCOUT READY` handshake)
2. `block_driver` with `CAP_MMIO | CAP_PCI`
3. `net_driver` with `CAP_MMIO | CAP_PCI`, then lends it the console for 1 s
4. `sfs_server` with `CAP_MMIO`, then waits (bounded) for `SFS READY`
5. `net_server` with `CAP_MMIO`, passing the driver's task ID
6. `gpu_driver` with `CAP_MMIO`, then lends it the console for 1 s
7. `input_driver` with `CAP_PCI | CAP_MMIO`, then lends it the console for 1 s
8. Howl, then transfers the console
9. Howler with `CAP_MMIO | CAP_IRQ` and Howl's ID as its input target

`gpu_driver` registers the name `gpu_driver` before the input driver is spawned,
so `input_driver` resolves it with `SYS_LOOKUP` on its first pointer sample.

### Graphics driver (`userspace/src/bin/gpu_driver.rs`)

- Resolution 640x1136 (portrait), 32-bit ARGB. The frame buffer comes from
  `SYS_ALLOC_DMA` (Normal Non-Cacheable, `(vaddr, paddr)`); `SYS_MAP_PHYS` maps
  only the `fw_cfg` MMIO window, because `map_phys_to_user` rejects DRAM. The
  same split would hold for a real PL111: registers are MMIO, the scan-out
  buffer is RAM.
- SilverHome is zero-copy: it creates a kernel surface
  (`SYS_SURFACE_CREATE`), draws pixels directly through
  `userspace/src/framebuffer.rs`, grants the compositor read/scanout access and
  sends `GUI_SURFACE_ATTACH`. The compositor re-points `ramfb` at that physical
  range; no blit. Installed apps still use the command stream into an offscreen
  surface, and `PRESENT` copies that surface to the framebuffer.
- The compositor's old engineer menu remains as a fallback when no client has
  attached.
- `fw_cfg` MMIO is at `0x09020000` (`data` @+0x00, `ctl` @+0x08, `dma` @+0x10).
  Transfers use the DMA interface only: a 16-byte `FWCfgDmaAccess` struct is
  written to a scratch DMA page, then a single 8-byte store to the `dma`
  register (big-endian, so `value.swap_bytes()`) triggers the transfer. Byte
  stores to the `dma` register are rejected by QEMU and surface as
  `EL0 SYNC FAULT` (`ESR_EL1=0x92000050`, DFSC=0x10 external abort) — the
  register accepts only 4- or 8-byte stores.
- `find_ramfb_selector` scans the `fw_cfg` file directory (key `0x19`, 64-byte
  big-endian entries) for `etc/ramfb`; `configure_ramfb` writes the big-endian
  `RAMFBCfg { addr, fourcc=0x34325241, flags, width, height, stride }`.
- Drawing helpers: `clear_screen`, `draw_rect` (1 px border), `draw_text` /
  `draw_text_centered` over an embedded public-domain 8x8 font. The menu draws
  three grey status cells ("Num", "No Data", "Date"), a 3x2 grid of blue buttons
  ("Phone", "SMS", "Web", "Media", "Power", "Settings") and a bottom panel with
  "SN: 877..." and "Silver Microkernel v0.1".
- Live UI loop: `serve()` blocks on `SYS_RECEIVE`, decodes `InputMsg`, moves the
  cursor, tracks `pressed_btn`, and repaints the whole frame (clear + menu +
  cursor) after every report boundary. A held button fills dark blue
  (`0x003366`); releasing restores the normal blue and prints `Button clicked!`.
- `draw_cursor` draws an 8x8 white arrow last, on top of the menu.

### Input driver (`userspace/src/bin/input_driver.rs`)

- PCI scan matches vendor `1af4` device `0x1052` (`VIRTIO_ID_INPUT`, modern-only;
  there is no transitional virtio-input). Its 64 KiB BAR window is assigned at
  `0x1030_0000` (after the block `0x1010_0000` and net `0x1020_0000` windows).
- Single event queue (queue 0). QEMU creates a 64-entry queue; the driver
  provides 64 buffers of 8 bytes each (one `virtio_input_event =
  { type: u16, code: u16, value: u32 }` per buffer, all little-endian).
- QEMU's `virtio_input_send` queues events and flushes one batch per
  `EV_SYN`/`SYN_REPORT`, popping one available buffer per event and firing an
  IRQ. The driver polls the used ring every 10 ms (`sleep(1)`), so no virtio
  IRQ is wired.
- Event handling: `EV_ABS` code 0x00/0x01 updates raw X/Y, `EV_KEY` code
  `0x110` (`BTN_LEFT`) updates the click state, `EV_SYN` code 0 forwards one
  sample. Raw absolute values `0..=32767` are scaled to `0..=639`/`0..=479`.
- Each `EV_SYN` sends one `InputMsg` to `gpu_driver`, resolved once via
  `SYS_LOOKUP("gpu_driver")` and cached; `SYS_TRY_SEND` drops samples if the
  mailbox is full rather than blocking the poll loop.

### Shared protocol (`userspace/src/input_proto.rs`)

`InputMsg { x: u16, y: u16, left_clicked: bool }` packed into an `OP_POINTER` (1)
IPC message (`x`, `y` little-endian, `left_clicked` in `payload[5]`). A sample is
small enough for the 32-byte mailbox, so it travels over IPC directly — no DMA
mailbox is needed (unlike `net_ipc`).

### QEMU flags

`build_and_qemu.sh` replaced `-nographic` with:

```text
-device ramfb
-device virtio-tablet-pci,disable-legacy=on,id=tablet0
-display gtk,show-cursor=on
-serial mon:stdio
```

### Verification

Boot log shows `GPU DRIVER: ENGINEER MENU DRAWN` and `VIRTIO-INPUT READY`.
Headless verification drives the pipeline with QMP `input-send-event` (each
command ends in an implicit `EV_SYN`; there is no explicit `sync` event type),
then `screendump` plus pixel sampling confirmed: the cursor moves to the target
and leaves no trail; pressing over "Phone" fills it `(0,51,102)`; releasing
restores `(0,90,181)` while "SMS" stays normal.

### Limitations recorded for the next phase

- The fallback engineer menu is single-buffered: every sample repaints the whole
  frame (clear + menu + cursor). The primary SilverHome path is double-buffered
  when two surfaces fit, but installed apps still draw through the command
  stream and a full-frame blit; present fences are best-effort `try_send`, not
  a kernel wait object.
- Input is polled at ~100 Hz, not IRQ-driven; virtio-input MSI-X/INTx is not
  wired. Absolute scaling assumes QEMU's `0..32767` range (the device-config
  `ABS_INFO` min/max is not read).
- The input driver's BAR is fixed at `0x1030_0000` (assumes block `0x1010_0000`
  and net `0x1020_0000`).

## State as of 2026-10-28 (Phase 4 complete: VirtIO-Net, smoltcp, shared-memory IPC and shell ping)

**Phase 4 (Network) is complete and verified end-to-end in QEMU.** The shell
drives the stack:

```text
howl> ping 8.8.8.8
PING 8.8.8.8: OK
```

That line appears only after the whole path succeeds: `net_driver` finds and
initializes VirtIO-Net; `net_server` allocates an 8 KiB DMA window and announces
it; the driver maps the window and returns the NIC MAC; Howl parses `8.8.8.8`
with the strict dotted-quad parser and sends `OP_PING`; smoltcp resolves the
slirp gateway with ARP; the echo request crosses the TX mailbox and the TX
virtqueue; the reply crosses the RX virtqueue and the RX mailbox; and smoltcp
accepts it with a valid IPv4/ICMP checksum. `net_server` owns no console, so it
relays `OP_PING_REPLY` to Howl, which prints the readable line.

`net_driver` (EL0) owns the VirtIO-Net PCI function; `net_server` (EL0)
runs smoltcp. The two exchange only doorbells, and frame bytes travel through
one shared DMA mailbox (see below). The shell reaches the stack over ordinary
IPC with `OP_PING` / `OP_PING_REPLY`.

### Fixes landed in this phase

- **PCI scan no longer drops transitional virtio-blk.** `find_virtio_blk()`
  matched `1af4:1001` but discarded it whenever BAR0 was an I/O BAR. BAR0 no
  longer gates the match; it is mapped lazily only when a modern capability
  actually references it. (Full diagnosis in the 2026-10-27 section below.)
- **`ping` timeout race.** The shell waits 3 s (`PING_WAIT_TICKS`) while
  `net_server` times out its own wait at 2 s (`PING_TIMEOUT_MILLIS`), so the
  server's timeout reply always has a full second to reach the shell before it
  gives up.
- **Strict IPv4 parser.** `parse_ipv4` accepts exactly a dotted quad of four
  decimal octets 0–255 (no spaces, at most three digits each); anything else is
  rejected with a usage message.

### Current boot chain

`silverd` starts, in order:

1. Scout (bounded `SCOUT READY` handshake)
2. `block_driver` with `CAP_MMIO | CAP_PCI`
3. `net_driver` with `CAP_MMIO | CAP_PCI`, then lends it the console for 1 s
4. `sfs_server` with `CAP_MMIO`, then waits (bounded) for `SFS READY`
5. `net_server` with `CAP_MMIO`, passing the driver's task ID
6. Howl, then transfers the console
7. Howler with `CAP_MMIO | CAP_IRQ` and Howl's ID as its input target

Task IDs are runtime values, never API. The network server resolves the driver
from the task list (preferring the spawn hint only while it is live). The pair
is restart-tolerant: a restarted `net_server` re-attaches, and a restarted
`net_driver` is rediscovered by the server.

### VirtIO-Net driver (`userspace/src/bin/net_driver.rs`)

- PCI scan accepts vendor `1af4` with device `0x1000` (transitional) or `0x1041`
  (modern-only); both are driven through modern common/notify capabilities. The
  legacy I/O BAR is never used.
- Its 64 KiB BAR window is assigned at `0x1020_0000`; the block driver keeps
  `0x1010_0000`.
- Split virtqueues: RX is queue 0, TX is queue 1, with 16 descriptors of 2 KiB
  per queue. Every frame carries the 12-byte modern virtio-net header; the
  driver sends a zeroed header and skips it on receive.
- There is no virtio-net interrupt. `serve()` calls
  `receive_timeout(..., IDLE_POLL_TICKS = 1)` (10 ms), so IPC wakes it early and
  otherwise it polls `receive_packet()` once per tick. This is polling, not
  IRQ-driven networking.
- `OP_ATTACH`: reject windows smaller than `DMA_SIZE`; map with `SYS_MAP_PHYS`;
  store `{vaddr, paddr, size, server}`. Re-announcing the same window keeps the
  mapping and refreshes the server task ID; it is not mapped twice. Reply with
  `OP_ATTACHED` and the MAC.
- `OP_TX`: accept only from the recorded server task ID; read `TX_LENGTH`; copy
  at most `MAX_FRAME` bytes into a static buffer; clear `TX_LENGTH` *before*
  `send_packet`, so the server may publish the next frame without racing the
  virtqueue.
- RX: only when `RX_LENGTH == 0`; copy one frame into `RX_FRAME`, `dmb ish`,
  publish the length and send `OP_FRAME`.
- RX and TX copies use static frame buffers; the userspace stack was 8 KiB at
  the time and is 64 KiB now (the TLS client needs the headroom).

### Shared-memory frame protocol (`userspace/src/net_ipc.rs`)

`net_server` allocates `DMA_SIZE = 8192` through `SYS_ALLOC_DMA`; the kernel maps
it Normal Non-Cacheable in both address spaces. IPC carries only control
messages and doorbells; an Ethernet frame never enters the 32-byte mailbox.

| Offset | Contents |
| --- | --- |
| `0x000` | `u32` RX length, written by the driver |
| `0x004` | `u32` TX length, written by the server |
| `0x800` | `MAX_FRAME` (1536) bytes, driver -> server |
| `0x1000` | `MAX_FRAME` (1536) bytes, server -> driver |

| Opcode | Direction | Meaning |
| --- | --- | --- |
| `OP_ATTACH` (1) | server -> driver | DMA physical address and size |
| `OP_ATTACHED` (2) | driver -> server | window mapped; NIC MAC follows |
| `OP_FRAME` (3) | driver -> server | a frame is in the RX slot |
| `OP_TX` (4) | server -> driver | a frame is in the TX slot |
| `OP_PING` (5) | shell -> server | one ICMP echo request; `payload[1..5]` is the IPv4 address |
| `OP_PING_REPLY` (6) | server -> shell | `payload[1]` is success, `payload[2..6]` echoes the address |

Publication rule: payload stores, then `dmb ish`, then the length store. The
consumer loads the length, `dmb ish`, copies the frame, then clears the length.
There is exactly one slot per direction. A producer publishes only when the
length word is zero, so a slow consumer applies backpressure instead of
overwriting a frame. TX doorbells from any task other than the attached server
are ignored; an oversized or zero TX length is reset and dropped.

### smoltcp server (`userspace/src/bin/net_server.rs`)

- `no_std`/`no_alloc`, with `medium-ethernet`, `proto-ipv4`, `socket-icmp` and
  `socket-udp`. A static `FrameDevice` holds one maximum-size (1536-byte) frame
  buffer per direction; the interface plan is IPv4 only.
- Starts with QEMU's default MAC, `10.0.2.15/24` and default route `10.0.2.2`.
  `OP_ATTACHED` replaces the MAC with the real one read by the driver. The MAC
  must not be hardcoded: ARP would otherwise use the wrong source address.
- Allocates and zeroes both length words before the first attach, and retries
  `OP_ATTACH` once a second until `OP_ATTACHED`. It no longer pings at boot:
  the ICMP socket (identifier `0x5349`) is idle until the shell sends `OP_PING`.
- On `OP_PING`, the server queues one ICMPv4 echo request to the given address.
  smoltcp performs ARP first and keeps the echo queued in the socket until the
  neighbor resolves. Only one echo is pending at a time: the ICMP socket is
  identified, so every pending request would match every reply.
- Main loop: poll the RX length word, hand the frame to smoltcp, clear the word,
  run `interface.poll`, drain ICMP replies, publish at most one TX frame when
  the slot is free, then sleep 10 ms. The loop also drains IPC doorbells so the
  driver's mailbox cannot fill up; frame delivery does not depend on a doorbell
  arriving.
- A pending echo that outlives its two-second window (`PING_TIMEOUT_MILLIS =
  2000`) is answered as a timeout. The result is reported to Howl as
  `OP_PING_REPLY` (success bit + the echoed address); the shell prints
  `PING <ip>: OK` or `PING <ip>: TIMEOUT`. Arbitrary network/server diagnostics
  are still discarded because `net_server` owns no console.
- A missing reply is never printed as success: the server answers a timeout
  itself, and the shell additionally checks that the echoed address matches.

### Verification

```bash
./build_and_qemu.sh release
```

Expected observable result at the `howl>` prompt:

```text
howl> ping 8.8.8.8
PING 8.8.8.8: OK
```

`ps` shows live `net_driver` and `net_server` tasks. `build_and_qemu.sh` does not
pass an explicit NIC; the `virt` machine's implicit default `virtio-net-pci`
provides QEMU user-mode networking with guest `10.0.2.15`, gateway `10.0.2.2`.
If the NIC were absent, the driver would exit with a stage-coded status and
`net_server` would keep retrying the attach.

### Network limitations recorded for the next phase

- IPv4 only, static address/gateway; no DHCP, DNS, IPv6 or TCP/UDP services.
- One frame slot per direction; no zero-copy, scatter-gather or multi-queue.
- RX is polled, not interrupt-driven; latency and throughput are bounded by the
  10 ms loop. No virtio-net IRQ is wired.
- `ping` is one-shot and single-flight: one ICMP echo request at a time, no
  timing statistics or continuous mode. There is no detach/unmap protocol: if
  `net_server` dies, the driver keeps the old mapping until a new attach.

## State as of 2026-10-27 late evening (BAR0 fix: transitional virtio-blk; SFS mounts end-to-end)

**The `driver_status=258` mystery is solved and SFS mounts.** A release boot
now prints `SFS MOUNTED`, `sfsinfo` reports magic `0x53465321` and `ls` lists
`bin/ etc/ tmp/` — the whole IPC/DMA path works end-to-end.

Root cause of `258` (`payload[2, 1]` = NOT_INITIALIZED, stage NOT_FOUND):
QEMU 11's default `virtio-blk-pci` is a **transitional** device. It reports
device id `0x1001` (so the vendor/device match was always correct) but routes
the legacy region into **BAR0 as an I/O BAR** (bit 0 = 1) and keeps the modern
common/notify capabilities in BAR4. `find_virtio_blk()` read BAR0 and did
`if bar0 & 0x1 != 0 { continue; }` — discarding the device it had just
matched, even though BAR0 never participates in the capability-driven setup.
The scan therefore "found nothing" → stage 1.

Diagnosis path worth repeating (driver prints are discarded without the
console):

- `qemu-system-aarch64 ... -monitor unix:/tmp/qmon,server,nowait` and
  `info pci` shows vendor/device ids and BAR types directly (`BAR0: I/O`,
  `BAR4: 64 bit prefetchable`). It also revealed QEMU's implicit default
  `virtio-net-pci` (`1af4:1000`) on the bus, which the driver correctly skips.
- The ECAM hardcode is **not** a bug: the QEMU virt DTB says
  `reg = <0x40 0x10000000 0 0x10000000>` (base `0x4010000000`, 256 MB, buses
  0–255), exactly matching `mmu::PCI_ECAM_BASE/SIZE`.

Fix applied in `userspace/src/bin/block_driver.rs`: BAR0 no longer gates the
match. `ASSIGNED_BAR0` and `assign_bar0()` are gone; BAR0 is mapped lazily via
a new `bar0_base()` helper (rejects I/O BARs) only when a capability actually
references BAR0 (modern-only devices), and the `_start` print now reports the
PCI BDF instead of "AT BAR0".

**Second, separate issue found by the fix:** with the driver alive, mount
still failed with `code=7` (`STAGE_BAD_MAGIC`, `reads=20`) — because
`disk.img` was not SFS-formatted at all (sector 0 started with the `dead beef`
pattern). **`disk.img` must be formatted once with `python3 format_sfs.py`**
after `run_disk.bat` or any other reset of the image; `run_disk.bat`
recreates it with the `DEADBEEF` pattern, which is not a superblock. Both
images were identical `dead beef` at the time of writing, so no data was
lost; formatting is safe and `disk.img` is git-tracked.

Also in this session: `build_and_qemu.sh` verification requires killing stale
QEMU processes (a running instance holds the `write` lock on `disk.img`), and
this environment's `pkill` is shadowed by an AppImage wrapper — use
`kill -9 <pid>` with a PID from `fuser disk.img` instead.

## State as of 2026-10-27 evening (stage-code exit diagnostics; driver init under suspicion)

`sfs_server` loads fine through the ELF loader but exits during mount. A
release boot printed:

```text
EXIT: task=7 program=11 status=8589939716 (0x000000020001404)
```

Decoded: stage 4 (`STAGE_READ_REJECTED`), attempts 20, reads 0,
`last_status` 2. **That 2 is NOT `VIRTIO_BLK_S_IOERR`.** In the block
driver's reply protocol `payload[0] == 2` means *queue not initialized*
(`QUEUE == None`): the driver answers from its IPC loop before any virtio
request is ever submitted, so QEMU never saw the request and no device-level
I/O error happened. The virtio descriptor chain in `read_sector`, the u64
sector encoding (`virtio_blk_outhdr` at offset 8, IPC wire
`payload[0]=opcode, payload[1..9]=sector`) and the volatile buffer zeroing
(separate 4 KB `alloc_dma` page; the virtqueue is a separate block) were all
audited and are correct — and unreachable until the queue exists.

Changes made to find the failing driver init step:

- `userspace/src/bin/sfs_server.rs`: mount failures now exit with
  stage-coded statuses. `status & 0xFF` names where mount gave up
  (`1..=7`: NO_DRIVER, SEND_FAILED, NO_REPLY, READ_REJECTED, SHORT_WINDOW,
  MAP_FAILED, BAD_MAGIC); bits 8-15 attempts, 16-23 reads, 32-47 the last
  driver status word. On a reply of 2 the status word packs the driver's
  init failure stage into its high byte.
- `userspace/src/bin/block_driver.rs`: `INIT_FAILURE` plus init stage codes
  (1 NOT_FOUND, 2 BAD_VERSION, 3 FEATURES, 4 NO_QUEUE, 5 QUEUE_SETUP); the
  "not initialized" reply is now `[2, stage]` so the client's exit report
  pinpoints the driver step. The spec-violating
  `notify_off_multiplier == 0 → None` rejection in `prepare_virtqueue` was
  removed: a zero multiplier is legal (all queues notify through one
  register; the written queue index selects the queue) and the notify
  address formula already handles it. This guard is the prime suspect for
  the queue never appearing on some QEMU builds.
- `build_and_qemu.sh` (new): builds the kernel + embedded userspace ELFs
  (release by default, `debug` arg) and boots QEMU with `disk.img` attached
  as `virtio-blk-pci`. It also prepends rustup's toolchain bin dir to PATH
  when cargo is not reachable (IDE sandbox shells). `--no-run` builds only.
  **A QEMU invocation without `-device virtio-blk-pci` produces the exact
  same EXIT signature as a driver init failure** — always boot with the
  disk attached.

Next step: boot via `./build_and_qemu.sh` and read the new EXIT status —
bits 40-47 name the driver init stage (1 = no virtio-blk device in the
command, 3 = feature negotiation, 5 = queue setup/`alloc_dma`), or `SFS
MOUNTED` if the multiplier fix was the whole story.

## State as of 2026-10-27 (SFS mounts; `sfsinfo` still mismatched)

**The SFS superblock now mounts.** A release boot shows neither
`SILVERD: SFS MOUNT FAILED` nor `SILVERD: SFS SERVER NOT READY, CONTINUING`,
which means `await_sfs_ready` received `SFS READY` and `sfs_server` entered
`serve()`. The client reads the DMA window correctly (magic `0x53465321`
matched), so the "client sees zeroes / wrong sector" symptom is gone.

Four independent defects had to be fixed to get there; only the last one is
still open for `sfsinfo`.

1. **The DMA window was zeroed *after* the device wrote it.**
   `read_sector` cleared the 4 KB request buffer with
   `core::ptr::write_bytes`, a non-volatile store. LLVM is allowed to sink it
   past the volatile completion polling, and in optimized builds it did exactly
   that: the sector the device had just transferred was wiped before the client
   mapped the window. Debug builds looked fine, release builds returned zeroes.
   It now clears with `write_volatile` in 8-byte units followed by
   `compiler_fence(SeqCst)`, and `read_superblock` fences after its volatile
   loads so the magic check cannot be hoisted above them.
2. **`MAIR_EL1` selected the wrong attribute bytes.** `MAIR_VALUE` was
   `0x00_44_FF`, i.e. index 1 (`ATTR_DEVICE`, used for MMIO) resolved to `0x44`
   Normal Non-Cacheable and index 2 (`ATTR_NORMAL_NC`, used for the DMA window)
   resolved to `0x00` Device-nGnRnE — the two attributes were swapped. The value
   is now `0x44_00_FF`: index 0 `0xFF` Normal WB, index 1 `0x00` Device-nGnRnE,
   index 2 `0x44` Normal Non-Cacheable.
3. **Stale translations on remap.** `map_user_pages` relied on one global
   flush after installing L3 descriptors. It now cleans the L3 line
   (`clean_table_lines`, `dc cvac` + `dsb ishst`) and then invalidates each page
   by address (`invalidate_user_page`, `tlbi vaae1is`) instead of a single
   `flush_after_table_change`. The address form keeps the rest of the TLB warm
   and cannot be silently skipped when a page table was just installed.
4. **Block-driver request layout disagreed with the client.** The client put
   the sector at `payload[0..8]` and the driver read `payload[..8]`, while every
   other option is prefixed with an opcode. The wire format is now explicit on
   both sides: `payload[0] = OP_READ_SECTOR` (`= 1`),
   `payload[1..9] = sector.to_le_bytes()`. Reply unchanged: `payload[0] = 1` on
   success, `payload[1..9]` = DMA window physical address, `payload[9..11]` = 512.


### Exception path: EL0 fault diagnostics are Rust again

The EL0 sync vector had `pl011::write_str` / `write_hex_u64` calls spliced into
its `global_asm!` block. Rust calls cannot appear in an assembly template, so
that block could never assemble (the strings would have been fed to the
assembler as instructions). The dump now lives in `silver_handle_task_fault`,
which already receives `esr` / `elr` / `far` in x0-x2 per AAPCS64 from a
four-instruction vector body:

```asm
exception_user_fault:
    mrs x0, esr_el1
    mrs x1, elr_el1
    mrs x2, far_el1
    b silver_handle_task_fault
```

A fault prints one block and then kills only the guilty task:

```text
EL0 SYNC FAULT!
ESR_EL1=000000009200000E
ELR_EL1=0000000040005AF0
FAR_EL1=FFFF000040000000
SWITCH TRACE from,to,sp,elr:
15->1 sp=FFFF000040740000 elr=FFFF00004009037C
```

`scheduler::dump_switch_trace()` is called again before `exit_current()`, so
the 16-entry ring is no longer dead code. Labels use fixed-width uppercase hex
and `\r\n`, like every other kernel dump.

### Open issue: `sfsinfo` gets opcode 5 instead of 0

`SFS NOT MOUNTED` is printed when the reply's `payload[0] != 1`. A probe that
dumps all 32 reply bytes returns, on a release run:

```
howl> sfsinfo
SFS NOT MOUNTED
  +0  0x0000000000050500
  +8  0x0000000000000000
  +16 0x0000000000000000
  +24 0x0000000000000000
```

The server-side probe fills `payload[1] = request.payload[0]`,
`payload[2] = request.sender as u8`, `payload[3] = OP_INFO`, so the reply decodes
as `payload[0] = 0` (status: not mounted), `payload[1] = 5`, `payload[2] = 5`,
`payload[3] = 0`. The server therefore saw `request.sender == 5` (correct: the
shell is task 5) but `request.payload[0] == 5` — the sender ID, where the shell
had written `0`.

**Fix applied:** In `userspace/src/bin/silvershell.rs`, `sfs_info()` now explicitly
sets `request_payload[0] = 0` before sending the request, so the server can
properly set `payload[0] = 1` (mounted) in its reply without echoing the sender ID.

---

## Open issue: Block driver DMA allocation (reads=0 in Release)

After fixing the `sfsinfo` 8-byte shift, the `sfsinfo` command now prints
`SFS SERVER NOT RUNNING`. In the loading log, `reads=0` is observed, meaning
`read_sector` returns `false` before any sector read is attempted.

**Root cause:** `syscall::alloc_dma(4096)` in the block driver returns 0 in
Release mode. This triggers the `if request_paddr == 0 { return false; }` check
added to prevent writing to the zero DMA page. The DMA allocation failure may be
due to:
- Frame allocator not properly initialized (all pages marked used, none free)
- Task lacking `CAP_MMIO` rights required for DMA allocations
- No contiguous physical memory available for DMA

**Workaround:** The block driver now checks if `REQUEST_BUFFER` was successfully
allocated and handles the failure gracefully by returning `false` from `read_sector()`
with a diagnostic message. The driver will not be able to read sectors until DMA
allocation is resolved.

**To debug:** Run the kernel in QEMU with debug output to determine whether the
frame allocator has free pages, whether the block driver task has `CAP_MMIO`, and
whether `alloc_dma_for_current` succeeds or fails and why.

---

Four independent defects had to be fixed to get SFS mounting; the DMA allocation
issue is the remaining open problem for block device support. { sender: u64, payload: [u8; 32] }` — identical.
- `sys_send` copies `size_of::<Message>()` bytes from the user pointer and then
  overwrites `sender` with the current task ID; `sys_receive` copies the same
  40 bytes back. `user_memory::copy_from_user` / `copy_to_user` are byte-wise
  with `+ index` addressing, so neither can shift the payload.
- The reply direction has no shift: the shell's dump is byte-for-byte what the
  server's probe wrote.
- Task table on this build (`ps`): `0 kernel`, `1 silverd`, `2 scout`,
  `3 block_driver`, `4 sfs_server`, `5 howl`, `6 howler`. `sfs_server` is task 4
  and the reply really comes from it (`receive_from` filters on
  `buffer.sender == task_id`).

The decisive experiment is staged in the working tree but **has not been run**:
the shell sends `[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, ...]` and the
server echoes `request.payload[..8]` into the reply's `payload[8..16]`. The `+8`
word of the dump gives the shift directly:

- `0x0000776655443322` — the server received exactly what was sent; then the bug
  is specific to the one-byte `&[0]` request path, not to marshalling.
- zeroes, or a variant with the bytes shifted by one — a real shift; keep the
  `payload[0] = opcode` convention and move the affected field accordingly.
- a word containing `0x05` — the sender ID is being substituted for
  `payload[0]` somewhere between `sys_send` and `serve()`.

Until that is settled `sfsinfo` reports `SFS NOT MOUNTED` even though the
filesystem itself is mounted.

### Verifying a boot (harness)

`run_disk.bat` must **not** be used for SFS work: it recreates `disk.img` with a
`DEADBEEF` pattern and boots the debug kernel. Use the Python harness instead —
it attaches the existing `disk.img`, waits for the `howl>` prompt and sends the
command terminated with `\r\n` (a bare `\r` never executes):

```powershell
Get-Process qemu-system-aarch64 -ErrorAction SilentlyContinue | Stop-Process -Force
cd 'F:\My Projects\Silver microkernel'
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std
python 'C:\Users\Public\sfs_check.py' 'target/aarch64-unknown-none/release/silver_microkernel' 'sfsinfo' 12
```

A hanging QEMU keeps `disk.img` locked, so always kill it first.
`C:\Users\Public\sfs_check.py` takes `<kernel-elf> <command> <seconds>`.

Note for any new probe: `syscall::print` from a task that does not own the
console silently returns 0, so `block_driver` and `sfs_server` cannot print
anything. Probes must travel back inside the IPC reply payload.

### Temporary probes in the working tree (remove before committing)

- `userspace/src/bin/block_driver.rs`: a `BLOCK SECTOR 0x…` `syscall::print`
  (invisible, harmless); the reply payload is **27 bytes** instead of 11, with
  `payload[11..15]` = first four bytes the driver reads back (`data[..4]`) and
  `payload[15]` = the sector it parsed.
- `userspace/src/bin/sfs_server.rs`: `MountFailure.driver_view` /
  `driver_sector` and their use in `announce` (`payload[26..30]`, `payload[30]`
  replacing the DMA physical address), plus the `serve()` else branch that
  echoes `request.payload[0]`, `request.sender` and `OP_INFO` and copies
  `request.payload[..8]` into `payload[8..16]`. Restore the else branch to
  `ipc::Message::new(0, &[0])` and the 11-byte success payload.
- `userspace/src/bin/silverd.rs`: prints `DRIVER 0x… SECTOR 0x…` from
  `payload[26..30]` / `payload[30]`; restore the `PADDR` print from
  `payload[26..32]` once `driver_view` / `driver_sector` are gone.
- `userspace/src/bin/silvershell.rs`: the patterned request payload and the
  32-byte dump loop in `sfs_info`'s `SFS NOT MOUNTED` branch. Restore
  `ipc::Message::new(0, &[0])` and the single-line message.

Changes in these files that must stay: the volatile zeroing plus fence in
`block_driver::read_sector`, decoding the sector from `payload[1..9]`, the
`OP_READ_SECTOR` request header in `sfs_server::mount`, the fence in
`read_superblock`, and the four `mmu.rs` fixes above.


## State as of 2026-10-26 (DMA/PCI infrastructure and EL0 UART; historical)

> Boot order, SFS and network status in this section are historical. The
> 2026-10-28 section above is the current boot chain.

Architecture at that point: **silverd -> Scout, block_driver, Howl, Howler**.
Normal boot creates only silverd (task 1). Silverd spawns Scout, waits for its
`SCOUT READY` IPC handshake (bounded to 2 s, boot continues on timeout), spawns
block_driver with CAP_MMIO|CAP_PCI, then spawns Howl and transfers the console,
and finally spawns Howler with Howl's ID as its input target. With the current
spawn order that is Scout=2, block_driver=3, Howl=4, Howler=5, but **no
component depends on those numbers**: every cross-task reference is resolved at
runtime (see below). The block driver is a full Virtio-PCI block driver (modern
common/notify capabilities, split virtqueue, descriptor chain, `read_sector`,
shared DMA reply buffer) but it does not implement SFS; the filesystem server is
not started at boot.
Howler maps PL011 and binds INTID 33. Normal user RX and TX both pass through
Howler in EL0 (userspace): input messages are `[0, byte]`, while output messages are
`[1, length, up to 30 bytes]`. `SYS_PRINT`, `SYS_PRINT_HEX` and `SYS_PRINT_DEC`
chunk console output into nonblocking IPC messages to the current console sink.
The sink is resolved at runtime by `scheduler::console_sink_task()`: the live task
whose `bound_irq` is INTID 33. Task IDs must never be hardcoded here — a spawned
task that happens to reuse the old ID (e.g. `spawn block_driver` taking ID 4)
would otherwise swallow the console output into an undrained mailbox. The kernel
wakes the sink's event wait for TX as well as hardware RX and yields after each
chunk. Early diagnostics, `demo-tasks` and a dead-Howler fallback still use kernel
PL011; a full TX mailbox drops the remaining output instead of blocking.

Userspace follows the same rule. `silvershell` resolves the block driver with
`find_task_id(PROGRAM_BLOCK_DRIVER)` over `SYS_TASK_LIST`, skipping dead and
stopped tasks, and resolves Howler the same way (`HowlerLink`, refreshed at most
once per second) instead of trusting the boot argument on liveness alone. Both
resolutions prefer the **most recently spawned** matching task: a respawned
driver re-runs device setup and therefore owns the hardware, while an older
instance keeps a virtqueue the device no longer uses. The driver itself accepts
requests from any caller and answers `request.sender` from the kernel-built
header, and it abandons a client that neither acknowledges nor stays alive within
10 s, so a crashed client cannot wedge the device.

The QEMU virt DTB describes the PL011 interrupt as SPI 1, level-high, which is
GIC INTID 33. The kernel fallback uses PL011 RXIM and RTIM with a bounded
256-byte FIFO; its IRQ handler drains the hardware FIFO completely. `SYS_READ`
reads only that software queue and never polls hardware. The timer no longer
polls UART. Both the unbound fallback and Howler consume Ctrl-Z (`0x1A`) as a
stop request rather than forwarding it to Howl.

Keep `GICD_ICFGR = 0b00` for this level-sensitive interrupt. `0b10` means
edge-triggered and left input stuck during development. RX timeout (RTIM) is
required so a single keypress does not wait for the RX FIFO threshold.

The interactive profile still has one UART owner. `spawn` and `bg` create children
without it; `fg <cap>` resumes a stopped task or foregrounds a live one, then waits
for either exit or stop. Ctrl-Z changes the child to `Stopped`, returns the console
to the shell and wakes the foreground waiter. Howler invokes `SYS_STOP_CONSOLE =
66`; the kernel stops the current console owner and schedules if one was stopped.
`ps` reports state code 7 as `stopped`. `SYS_TASK_RESUME = 40` and
`SYS_WAIT_EVENT = 41` implement resume and the `Exited`/`Stopped` event split;
old `SYS_WAIT` still waits only for exit.

A stopped task keeps `resume_state`: `Running` becomes `Ready`, while sleep and
blocked waits retain their deadlines/queue membership. Expiry or notification
while stopped updates the saved state instead of accidentally scheduling the task.
Killing stopped/blocked IPC receivers wakes both `not_empty` and `not_full` before
mailbox destruction, preventing a dangling WaitQueue during reaping.

Focused security review found and fixed four concrete issues: `SYS_SEND` now
overwrites the user-provided `Message.sender` with the current task ID; mutex
unlock rejects non-owners in release as well as debug; nameserver decimal ID
parsing uses checked arithmetic; and sleep deadlines use saturating addition.
Regression role 27 verifies sender stamping, non-owner/owner unlock and survival
of a child sleeping for `u64::MAX` milliseconds. This was a focused review, not a
complete security audit. Task/memory/wait-queue/file resource quotas remain open.

Capability model implemented. Raw Task IDs are no longer sufficient for kill/wait/console.
`SYS_CAP_GRANT = 61` delegates a subset of an existing capability's rights to
another live task; recipients cannot receive rights the caller does not hold.
When a target dies, its capabilities lose every right except `CAP_WAIT`; that
wait-only capability is revoked when the completion record is consumed.
`SYS_MAP_PHYS = 62` requires `CAP_MMIO` and maps non-DRAM physical pages as
Device-nGnRnE into a first-fit 2 MB user window at `0x80000000`.
`SYS_BIND_IRQ = 63` requires `CAP_IRQ` and exclusively binds a non-timer INTID.
`SYS_WAIT_IRQ = 64` consumes a pending notification or blocks until one arrives.
`SYS_TRY_SEND = 65` rejects a full or dead mailbox without blocking.
`SYS_STOP_CONSOLE = 66` lets Howler deliver Ctrl-Z without forwarding the byte.
Bound level-triggered IRQs are masked before GIC EOI, then unmasked by
`bind_irq` and every `wait_irq` call to prevent an IRQ storm while EL0 handles
the device. Program 4 (Howler) receives self capabilities with CAP_MMIO and
CAP_IRQ at spawn, plus an explicit CAP_GRANT from silverd. It maps PL011, binds
INTID 33, sends input to Howl and writes output IPC to DR without entering the
kernel PL011 handler on the normal path.

`SYS_ALLOC_DMA = 67` requires CAP_MMIO, accepts a byte size in x0 and returns a
page-aligned user VA in x0 plus its physical DMA address in x1. Allocations are
zeroed contiguous frames, rounded to whole pages and limited to 2 MB total per
call. They share the first-fit mapping window at `0x80000000` with MMIO, never
the heap, and are mapped Normal Non-Cacheable (MAIR index 2, value `0x44`) rather
than Device memory. The kernel cleans the cacheable aliases before exposing the
buffer. During reaping it scans every live address space with
`mmu::revoke_phys_range`; tracked frames are released only when the scan proves
`safe_to_free`, otherwise they are leaked (`DMA_LEAKED_FRAMES`). That proof
covers CPU mappings only: a driver must stop/reset its queue and observe
completion before exit, because there is no IOMMU. No DMA-free syscall or
per-allocation quota exists yet.

`SYS_PCI_READ = 68` and `SYS_PCI_WRITE = 69` require CAP_PCI. Their offset is a
4-byte-aligned byte offset within QEMU virt's 256 MB ECAM at physical
`0x4010000000`; reads/writes are volatile 32-bit accesses through a kernel
Device-nGnRnE mapping. Program 10 (block_driver) receives self capabilities with
CAP_MMIO and CAP_PCI at spawn, plus an explicit CAP_GRANT from silverd. It walks
the ECAM bus/device/function space for vendor `0x1AF4` with device `0x1001` or
`0x1042`, maps BAR0/BAR4, negotiates VIRTIO_F_VERSION_1, allocates the split
virtqueue (descriptor table, available and used rings in one DMA allocation),
posts a `{type=0, sector}` request with inline 512-byte data and a status byte,
and replies over IPC with the shared DMA physical address plus length. The client
maps that buffer and releases it with a `0xA5` acknowledgement.

Verified in QEMU debug and release with `-device virtio-blk-pci`: `ps` showed
`scout`, `block_driver`, `howl`, `howler` as live tasks, `diskread` printed
`DISK SECTOR 0: 0xDEADBEEF...` and `hexdump 0` dumped the sector read from
`disk.img`. `spawn block_driver` created a second instance (ID 6) that re-ran
device setup; `diskread` then used it, `kill` of its CapId did not disturb the
shell, and `say`, `ls /`, `fastfetch`, `help` and the prompt kept working — a dead
block driver degrades to a printed error, never a hang, and kernel output still
falls back to PL011 when no console sink exists.

Debug and release QEMU passed Howler-backed TX/RX and sleeping `demo 12`
stop/kill/wait; debug also passed `fg` resume and `uname`. A twelve-second debug
`demo-tasks` run passed 18 markers with exactly two intentional `TASK FAULT`
reports (the same dump is labelled `EL0 SYNC FAULT!` since the exception-path
cleanup):

```text
CONDVAR TEST: beta silent = PASS
LIFECYCLE EXIT/UNREGISTER=PASS
LIFECYCLE MUTEX CLEANUP=PASS
COPYOUT READONLY REJECT=PASS
COPYOUT UNMAPPED REJECT=PASS
HEAP GROW/SHRINK=PASS
MALLOC FREE/REUSE=PASS
MALLOC BRK TRIM=PASS
GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS
GLOBAL ALLOC TRIM=PASS
IPC BP SENDER RESUMED=PASS
IPC TIMEOUT=PASS
IPC BP BACKPRESSURE=PASS
IPC EXIT WAKE=PASS
SECURITY IPC SENDER=PASS
SECURITY MUTEX NONOWNER=PASS
SECURITY MUTEX OWNER=PASS
SECURITY SLEEP OVERFLOW=PASS
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
same-path/missing-parent/self-subtree/active-cwd moves were rejected. Append
tests passed in both profiles with spaced/unspaced `>>`, existing and new files,
quoted literal operators and a `>` truncation regression check. ANSI prompt and
clear sequences were observed, and uname printed the exact version string. No
`KERNEL PANIC`, `BAD SP` or unexpected `FAIL` occurred. For the DMA/PCI change,
both debug and release `ps` showed a live block_driver task after its DMA
read/write and ECAM-read smoke checks; the 18-marker regression run still
had exactly the two intentional isolation faults.

| Probe | Current ID | Address read from EL0 | Expected FAR |
| --- | ---: | --- | --- |
| High-half kernel | 15 | `0xFFFF000040000000` | `FFFF000040000000` |
| Removed low kernel text alias | 16 | `0x0000000040080000` | `0000000040080000` |

Both have `ESR_EL1 >> 26 = 0x24`, a data abort from a lower exception level.
These are expected translation/permission failures, not kernel regressions.
IDs depend on spawn order and are not a stable userspace API.

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
8. **Scout RPC uses selective receive.** `receive_from(SCOUT_ID)` leaves
   unrelated messages queued. Plain receive could consume a PING as a lookup
   reply, lose the real reply and create a persistent WHAT? loop.
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
13. **Stopped tasks are woken only through `resume_state`.** `wake_blocked_task`
    and timer expiry update the saved state while leaving `Task.state = Stopped`.
    Only `resume_task` restores it. Never set a stopped task directly to Ready
    from a mutex/condvar/process notification.
14. **IPC queues must outlive their registered waiters.** `remove_task` notifies
    both `not_empty` and `not_full` before dropping a mailbox. This is mandatory
    for kill, including a receiver stopped while blocked on `not_empty`.

Always build like this:

```bash
cargo build --target aarch64-unknown-none --no-default-features --features no_std
```

For bare-metal profiles, `build.rs` invokes Cargo in `userspace/`, uses
`target/userspace` as its target directory, matches debug/release profile and
copies every userspace ELF into `OUT_DIR`. `src/user_program.rs` assigns IDs
0 (demo), 1 (silverd), 2 (Scout), 3 (Howl), 4 (Howler), 5 (ls), 6 (cat),
7 (echo), 8 (say), 9 (sl), 10 (block_driver), 11 (sfs_server),
12 (net_driver), 13 (net_server), 14 (gpu_driver) and 15 (input_driver).
`PROGRAM_DYNAMIC = u64::MAX - 1` marks dynamically loaded (`run`) tasks and is
not part of the table; `hello.elf` is built but only seeded into the VFS. Host
builds skip userspace. Normal boot starts
only silverd; `demo-tasks` starts Scout and the regression task set instead.

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
  The virt DTB describes UART as SPI 1 / level-high: GIC INTID 33, ICFGR `0b00`.
  RXIM + RTIM are unmasked; the RX threshold is 1/8 and the IRQ drains the FIFO.
- Timer: ARM Generic Timer, IRQ 30, 100 Hz tick (`timer::TICK_INTERVAL_MS = 10`).
  UART priority is 0x80 and timer priority 0xA0; ordinary RX does not schedule.
- PCI ECAM: physical `0x4010000000`, 256 MB, kernel-only Device-nGnRnE mapping.
- VirtIO-Net: QEMU `virt`'s implicit default NIC is a transitional `1af4:1000`
  (`1af4:1041` modern-only is accepted too). The driver assigns 64 KiB at
  physical `0x1020_0000`, uses modern common/notify capabilities, queues 0 (RX)
  and 1 (TX), 16 x 2 KiB buffers per queue, and the 12-byte modern virtio-net
  header. No virtio-net IRQ is wired; RX is polled every loop.
- fw_cfg: MMIO at physical `0x09020000` (`data` `+0x00`, `ctl` `+0x08`, `dma`
  `+0x10`). The `dma` register is big-endian and accepts only 4- or 8-byte
  stores; byte stores raise an external abort. `ramfb` (`-device ramfb`) is a
  plain RAM framebuffer configured through the `etc/ramfb` fw_cfg file with a
  big-endian `RAMFBCfg` (fourcc `0x34325241` = ARGB8888).
- VirtIO-Input: `-device virtio-tablet-pci` is a modern-only `1af4:1052` device
  with one event queue (queue 0, 64 entries, 8-byte little-endian events).
  Absolute X/Y are reported `0..32767`. The driver assigns 64 KiB at physical
  `0x1030_0000`.
- FP/SIMD enabled in `_start` (CPACR_EL1 = 0x300000, CPTR_EL2 = 0) —
  without this the first use of a float register traps with EC=7
- Runtime is EL1h for the kernel and EL0t for user tasks: QEMU `-kernel`
  starts at EL2, `_start` drops to EL1 via ERET (HCR_EL2.RW=1,
  SPSR_EL2=0x3C5). VBAR_EL1, both TTBRs, TCR_EL1, MAIR_EL1 and SCTLR_EL1
  are programmed before normal Rust subsystem initialization.
- Translation uses 48-bit VAs on both halves (T0SZ=T1SZ=16), 4 KB granules,
  40-bit PAs and four table levels. MMIO is device-nGnRnE; ordinary DRAM is
  normal WB inner-shareable; user DMA is Normal Non-Cacheable. MAIR entries
  are `0xFF`, `0x00`, `0x44` at indices 0, 1, 2. Data/instruction caches are
  off through the boot stub (SCTLR.C/I = 0) and are enabled by
  `mmu::init` -> `cache::init_boot` once the tables are published.

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

Six statically allocated boot tables have fixed 4 KB slots. The assembly fills
the first five; the PCI table is zeroed with `.bss` and populated by `mmu::init()`:

| Index | Table | Role |
| --- | --- | --- |
| 0 | low L0 | Temporary boot TTBR0; emptied after the high-PC transition |
| 1 | high L0 | TTBR1 root |
| 2 | shared L1 | MMIO gigabyte at index 0, DRAM gigabyte at index 1, PCI at index 64 |
| 3 | MMIO L2 | 512 × 2 MB EL1-only device blocks |
| 4 | DRAM L2 | 512 × 2 MB EL1-only normal-memory blocks |
| 5 | PCI L2 | 128 × 2 MB EL1-only device blocks starting at L2 index 128 |

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
- L1[2] → an on-demand L2/L3 chain for authorized MMIO and DMA mappings at
  `0x80000000`. These two table pages are allocated only on first use.
- There are no user mappings for kernel text/rodata, kernel `.data` or `.bss`,
  kernel heap, kernel stacks, unrelated DRAM, page-table pages or unauthorized
  MMIO.
- `destroy_task_space()` frees all task-owned tables, heap frames and the
  private page; `Task::release_space()` then releases ELF and tracked DMA frames.

## ELF loading and W^X

`userspace/linker.ld` links a static `ET_EXEC` image at `0x40000000` with
separate R-X text, R-- rodata and RW data `PT_LOAD` segments. `.bss` is
`NOLOAD` and belongs to the RW segment, so its memory size can exceed its
file size.

`elf::load_elf()` accepts ELF64 little-endian AArch64 `ET_EXEC` files only. It
checks the ELF header, program-header and file bounds, requires every loadable
segment to fit within the 2 MB image window, rejects W+X segments and rejects
different permissions sharing one 4 KB page. The entry must lie in an
executable segment. It allocates fresh contiguous frames, zeroes the whole
image, copies `PT_LOAD` file bytes and records a `PageKind` for every page,
returning an owned `LoadedImage { physical_start, page_count, entry, kinds }`.
`elf_loader::load()` is a thin wrapper over it that logs the rejection reason;
both the embedded `spawn` path and the dynamic `run <path>` path share this one
loader.

The resulting mappings enforce W^X: text is R-X at EL0 and PXN; rodata is R--
and UXN/PXN; data, heap, stack and private memory are RW at EL0 and UXN/PXN.
Each spawn loads another copy, so even writable ELF data/BSS is private. VM A/B
write `0xAAAA` / `0xBBBB` to their private page, `ELF_BSS_CELL` and heap, then
read back only their own values.

## EL0 execution and lifecycle model

- `SYS_SPAWN(program, argument)` selects any embedded program ID from the table
  above. `scheduler::spawn_user` creates a kernel stack, an 8 KB user stack, a
  private page and an independent ELF copy before constructing TTBR0. Only idle
  remains an EL1 task.
- A fresh user context has high SP_EL1, the ELF entry in x19, the argument in
  x20 and SP_EL0 = `0x7FE00000 + 0x2000`. `silver_user_bootstrap` moves x20 to
  x0, sets ELR_EL1 = x19 and SPSR_EL1 = 0, then executes `eret`.
- Demo roles are selected by the argument in `userspace/src/main.rs`, not by
  task ID. Silverd, Scout, Howl, Howler and utilities have separate executables
  in `userspace/src/bin/`.
- The scheduler switches only TTBR0_EL1, then performs
  `tlbi vmalle1; dsb ish; isb`. High kernel text and SP_EL1 remain valid
  through TTBR1 throughout the switch.
- The sync exception entry recognizes SVC by EC=0b010101. Other exceptions
  inspect SPSR.M[3:2]: EL0 goes to `silver_handle_task_fault`, which prints
  `Task <id> killed: Page Fault at 0x... (ESR: 0x...)` plus `ELR_EL1`, dumps
  the switch trace and kills only that task; EL1 goes to
  `silver_handle_exception`, which dumps kind/ESR/ELR/FAR and halts (kernel
  panic).
- Syscalls include sleep, IPC, nameserver, print, task control, spawn/args,
  dynamic ELF spawn (`SYS_SPAWN_PATH`), wait/kill, stop/resume, `brk`, UART
  read, console ownership and RAM-VFS.
- `SYS_CONSOLE_SET(cap_id)` requires `CAP_CONSOLE_GRANT`, then rejects idle,
  missing and dead target IDs. On success it makes the selected task the sole
  interactive UART owner under one IRQ-critical
  section and records the calling shell as `console_parent` for Ctrl-Z. Print
  syscalls from non-owners succeed but discard their bytes; non-owner `SYS_READ`
  returns SYSCALL_ERROR without touching PL011.
- On the normal path, console print syscalls copy and validate user bytes, then
  send `[1, len, data]` chunks to the console sink with `ipc::try_send`. The sink
  is looked up per call through `scheduler::console_sink_task()` (the live task
  bound to INTID 33), so a shell-spawned task cannot capture the output stream.
  Successful sends set the sink's pending event, wake its `wait_irq` and yield
  the CPU. Howler polls
  output IPC with zero timeout and waits for TXFF before writing DR. A dead
  Howler selects the kernel PL011 fallback; a full mailbox drops remaining bytes.
- The unbound PL011 fallback drains INTID 33 into a 256-byte software FIFO and
  consumes Ctrl-Z as a stop request. The owner's `SYS_READ` reads only that FIFO;
  no byte/no input returns SYSCALL_ERROR. On the normal path Howler reads DR,
  clears RX/RT interrupt sources and either sends `[0, byte]` to Howl or invokes
  `SYS_STOP_CONSOLE` for Ctrl-Z. Timer IRQs handle ticks and normal scheduling.
  `SYS_UPTIME` returns raw 100 Hz ticks. `SYS_MEMINFO` copies three u64 byte counts
  through checked copyout; used includes kernel/reserved RAM and allocated frames.
- `SYS_TASK_LIST(buffer, capacity)` copies up to 64 `#[repr(C)]` TaskInfo entries
  (ID, numeric state, program, argument). State 7 is Stopped. Task metadata lives
  in the Task itself, so ps can distinguish identical ELF roles.
- `SYS_SPAWN_ARGS(program, numeric, ptr, len)` copies at most 192 argument bytes
  before allocating the task. `SYS_ARGS(buffer, capacity)` returns those bytes
  to the current program and fails rather than truncating them.
- `SYS_WAIT(cap_id)` requires `CAP_WAIT`, blocks only for termination and consumes
  one of 64 bounded completion records. `SYS_WAIT_EVENT(cap_id)` instead returns
  Stopped as `SYSCALL_STOPPED = u64::MAX - 2` when the target is stopped.
  `SYS_TASK_RESUME(cap_id)` requires `CAP_RESUME`, restores the saved state and is
  a no-op for a live non-stopped task. `SYS_KILL(cap_id)` requires `CAP_KILL`,
  rejects idle/self and records status 1; SYS_EXIT records its status.
- `SYS_SEND(target, message)` blocks while the target has 16 queued messages and
  returns `SYSCALL_ERROR` if the target is dead. `SYS_TRY_SEND` has the same
  checked-copyin and sender-stamping path but returns immediately on a full or
  dead mailbox; the UART driver drops that byte and continues its IRQ loop.
  After copyin the kernel always replaces `message.sender` with
  `current_task_id()`, making selective receive and nameserver replies resistant
  to sender spoofing. Nameserver RPC paths propagate send failure and parse
  returned IDs with checked arithmetic.
- `SYS_SLEEP(ms)` uses a saturating absolute deadline, so an extreme userspace
  argument cannot panic a debug kernel or wrap to an immediate wakeup.
- `SYS_MUTEX_UNLOCK` succeeds only for the recorded owner in every build profile;
  non-owner attempts return `SYSCALL_ERROR` without waking a waiter.
- `SYS_RECEIVE(buffer, timeout_ticks)` uses zero for a poll and `u64::MAX` for an
  infinite wait. Other values are relative 100 Hz ticks; expiry returns
  `SYSCALL_TIMEOUT = u64::MAX - 1`. Invalid buffers still return SYSCALL_ERROR.
- `SYS_BRK(0)` queries the program break. Other values set an absolute break in
  `0x40200000..0x40400000`; invalid requests and allocation failures return
  `SYSCALL_ERROR` without changing the previous break or mappings.
- `SYS_MAP_PHYS(phys, size)` rejects empty, overflowing and DRAM ranges. Its
  lazily allocated L2/L3 tables are owner-registered and freed with the task
  address space; existing descriptors are revalidated against the walker's
  owner before they are followed, while referenced device frames are never
  freed. Each mapping records a `CAP_MMIO` region token and returns the user VA
  including the original page offset. A shared-DMA range is accepted when it
  lies inside some task's tracked allocation; the physical address is a bearer
  capability, so the IPC protocol that transmits it is the grant boundary.
- A bound IRQ bypasses the kernel device handler: the GIC distributor masks it
  before EOI, then a pending flag and wakeup go to its live owner. `bind_irq`
  and `SYS_WAIT_IRQ` unmask it only when EL0 is ready for another event.
  `SYS_WAIT_IRQ` preserves events delivered before the wait. Exit, kill and
  reaping clear the binding, restoring the kernel fallback; timer and spurious
  INTIDs cannot be bound.
- Exit and kill clear the task mailbox, wake both receiver and sender waiters
  before freeing its condvars, and close every VFS descriptor owned by that task.
  If the dead task owned the interactive console, `recover_console` gives it to
  the first live Howl before scheduling; `demo-tasks` instead only clears
  the dead task's shared-console flag. Reaping removes stale waiters, releases
  owned mutexes, wakes the next waiter, then frees ELF pages, heap frames, page
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
  Each handle has owner, per-owner fd (starting at 3), offset, writable and
  append flags. Exit/kill removes all handles for that owner.
- `SYS_FS_OPEN/READ/WRITE/CREATE/CLOSE/READ_DIR/MKDIR/UNLINK/RMDIR/RENAME` use
  checked copyin/copyout. `SYS_CHDIR` resolves and validates a directory before
  replacing the Task cwd; `SYS_GETCWD` copies its absolute bytes. Paths are
  limited to 191 bytes, names to 32 bytes and one I/O call to 4096 bytes.
  `create` truncates files, `open` supports read-only, write and write+append
  modes. Append starts at the current EOF on every write, not merely at open.
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
- There is no persistence or file-size quota yet. Repeated writes extend the
  in-memory file through the kernel heap.

## Howl (silvershell.rs)

- Howl is an independent no_std/no_main ELF with no allocator. It uses a
  96-byte line buffer, accepts printable ASCII and CR/LF, echoes input, and
  implements Backspace/DEL by removing the last byte and erasing it onscreen.
- Normal boot is automatic: silverd starts Scout, waits (bounded) for its
  `SCOUT READY` IPC, starts block_driver with PCI/MMIO rights, then starts Howl
  and hands it the console, and finally starts Howler with Howl's ID as its input
  target. There is no manual `start_uart_driver` command. Howler maps PL011,
  binds INTID 33 and sends `[0, byte]` input messages directly to Howl's mailbox.
  Howl polls with a zero timeout, accepts only type-0 messages from the Howler it
  resolved from the task table, and sleeps 1 ms when empty. That ID is re-resolved
  from `SYS_TASK_LIST` at most once per second, so a respawned Howler is adopted
  automatically and a dead one is detected — `silverd` never has to know Howler's
  number in advance. Normal output uses SYS_PRINT/SYS_PRINT_HEX/SYS_PRINT_DEC,
  whose typed chunks return through IPC to Howler for the actual PL011 writes.
- Startup emits ANSI clear/home, runs fastfetch once and shows a bold cyan
  `howl>` prompt. Commands are `help`, `fastfetch`, `ls`, `cat`, `echo`, `say`,
  `sl`, `run`, `mkdir`, `rm`, `rmdir`, `mv`, `cd`, `pwd`, `clear`, `uname`, `ps`,
  `spawn`, `bg`, `fg`, `kill` and `wait`; empty lines only reprompt. Echo parsing
  preserves quoted spaces, strips outer quotes and recognizes `>`/`>>` only
  outside quotes. `mv old new` takes a full destination path, not just a directory.
- `ls`, `cat`, ordinary `echo` and `say` spawn separate EL0 utilities with
  string args and inherited cwd. `say` preserves spaces and strips paired outer
  quotes. `sl` runs a 60-frame ANSI steam-locomotive animation. `run_utility`
  gives the child the console and uses wait_event for Exited/Stopped events.
  `echo text > path` truncates/creates; `echo text >> path` appends/creates
  directly through VFS. `mkdir`, `rm`, `rmdir`, `mv`, `cd` and `pwd` are direct
  syscalls.
- `ps` uses a fixed 32-entry userspace array and prints ID/program/argument/state,
  including `stopped`. `spawn` and `bg` parse an optional checked decimal u64 and
  create a task without console ownership. `fg <cap>` calls resume, transfers the
  console and waits for an Exited/Stopped event. Howler consumes Ctrl-Z and calls
  `SYS_STOP_CONSOLE`, so the foreground task stops without receiving byte 0x1A.
  `kill` works on stopped tasks, and plain `wait` still reports only exit status.
- Background task output is dropped rather than buffered, so early output cannot
  be replayed by `fg`. Only output produced after the transfer becomes visible.
- Fastfetch converts ticks to seconds with hundredths and bytes to tenths of a
  MiB. Its three memory counters come from SYS_MEMINFO, not hardcoded free RAM.
  The architecture row is `Kernel: AArch64`.
- Normal boot starts only silverd (task 1) in the kernel; silverd creates Scout,
  block_driver, net_driver, sfs_server, net_server, Howl and Howler in that
  order. Nothing in userspace or the kernel reads those numbers: the console
  sink, Howler, block_driver, net_driver and net_server are all resolved from
  live scheduler state. `demo-tasks` does not start silverd: Scout remains task 1,
  the earlier regression IDs are preserved and every demo opts into shared UART
  output.

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

1. **Single-CPU by default; QEMU SMP is opt-in.** Default builds keep
   `cpu_count() == 1`; fog and tablet force one CPU. The `smp` feature brings
   up to `MAX_CPUS` (4) QEMU APs online (GIC/timer/IPI) and APs execute tasks;
   `smp-selftest` adds the boot checks. The earlier single-CPU ownership
   proofs are replaced by a locking contract: the BKL serializes kernel state,
   the inner spinlocks (frame allocator, page-table registry, DMA quarantine)
   cover short mutations, and teardown/reap runs behind the quiescence gate
   (the SGI1 stop-the-world handshake when a dead task owns DMA) before the
   first PTE edit. Any new shared `UnsafeCell` wrapper must state which of
   those protects it before APs may touch it.
2. **Critical sections via the BKL, with DAIF as local belt-and-braces.**
   Global kernel state (scheduler, IPC) is mutated with the kernel lock held;
   kernel entry/IRQ paths also mask IRQs locally, but IRQ masking is not the
   cross-CPU mechanism. Blocking paths release the BKL before `schedule` and
   re-acquire on resume; `Mutex`/`Condvar` remain the task-level primitives.
3. **The DAIF rule for tasks.** A voluntary context switch (`sleep` /
   `yield` / `block_current_on`) inherits masked IRQs. Kernel task entry points
   must call `interrupts::enable_irq()` before waiting; idle does this.
   User tasks get unmasked PSTATE from `silver_user_bootstrap` and from the
   SPSR saved in their exception frames.
4. **Timer and UART IRQs have separate paths** (`gic::silver_handle_irq`).
   Timer: `timer::handle_irq()` → EOI → `schedule()`. A bound non-timer IRQ takes
   distributor mask → EOI → set pending/wake its user owner without entering
   the kernel driver; `bind_irq`/`SYS_WAIT_IRQ` unmask it for the next event.
   Unbound UART: `pl011::handle_irq()` drains RX → consume stop request →
   optional `stop_console_owner()` → EOI → `schedule()` only if a task stopped.
   Ordinary unbound UART bytes never cause a context switch. PL011 is
   level-sensitive: its ICFGR field must remain `0b00`.
5. **The heap is initialized after the frame allocator** and only through
   `alloc_contiguous_pages`. No `alloc` before `allocator::init()`.
6. **Context switch layout:** x19-x30 at offsets 0-95, SP_EL1 at 96,
   SP_EL0 at 104, resume PC (`TaskContext.elr`) at 112 — 120 bytes total.
   `silver_context_switch` saves/restores this block and finishes with
   `ret`. On save, x30 is copied into the resume-PC slot; on restore, the
   resume-PC slot wins over the saved x30. Fresh tasks put their entry or
   `silver_user_bootstrap` there.
7. **Task states:** `Ready / Running / Sleeping(deadline_ticks) /
   Blocked / BlockedUntil(deadline_ticks) / Stopped / Dead`. The scheduler gives
   the CPU only to Ready tasks. Stopped tasks keep a separate resume_state;
   timer expiry and wakeups update that saved state without scheduling them.
   `find_next_task` prefers non-idle tasks.
8. **Both trampolines keep ELR/SPSR on the stack.** Each builds a 272-byte
   frame: x0-x30, ELR at offset 248, SPSR at offset 256. The epilogue
   restores ELR/SPSR from the frame before `eret`. They mask IRQs on entry
   and do not need to re-arm ELR/SPSR for scheduling: the ret-based switch
   unwinds back to the epilogue with the original SP. CPU-global ELR/SPSR
   may be clobbered by other tasks in between; the frame copy is authoritative.
9. **SVC register preservation starts before ESR decoding.** x9/x10 are user
   registers, not free scratch. The sync entry saves/restores them around its
   dispatch check; the full 272-byte frame is built afterwards.
10. **Scout holds the well-known SCOUT_ID**, fixed by spawn order: 2 in normal
    boot and 1 in `demo-tasks`. Protocol: text over mailbox messages,
    "R <name>" → "OK"/"TAKEN", "L <name>" → "ID <n>"/"MISSING", and
    "U <name>" → "OK"/"MISSING"/"NOT OWNER". Kernel RPC uses
    `receive_from(SCOUT_ID)`, so unrelated queued messages are preserved.
11. **Cross-crate syscall structures have a stable layout.** `Message` is
    `#[repr(C)]` in kernel and userspace. Apply the same rule to future
    structs passed by pointer across the SVC boundary.
12. **TLB/cache ritual for any future table edit.** After modifying tables,
    clean lines to the point of coherency (`dc cvac` + `dsb`), invalidate
    stale TLB entries (`tlbi` + `dsb` + `isb`). Template:
    `mmu::flush_after_table_change`. Cache ranges are aligned and stepped from
    `CTR_EL0.DminLine` (`4 << DminLine`), not a hardcoded 64 bytes; keep start
    rounded down and end rounded up. Address-space switches load TTBR0_EL1
    and issue `tlbi vmalle1; dsb ish; isb` before the context switch.
13. **UART FIFO waits preserve DAIF.** `pl011::write_byte()` may unmask IRQs
    while waiting for TXFF, but restores the caller's saved DAIF before
    returning. Never replace this with an indefinitely masked spin. Taking
    a timer IRQ mid-print is intentional and safe with the frame-based
    trampolines.
14. **Fault diagnostics must not create a second fault.** Validate every
    userspace stack word before printing it; print `<unmapped>` at the first
    invalid word. (The SP_EL0 stack walk itself is not emitted right now — see
    Debugging aids — but the rule stands as soon as it comes back.)
15. **The network mailbox length word is the publication barrier.** Write the
    payload first, `dmb ish`, then the length; on consume, read the length,
    `dmb ish`, copy, then clear. The window is Normal Non-Cacheable; volatile
    accesses alone do not order the payload against the length.
16. **One frame slot per direction, with backpressure.** Publish only when the
    length word is zero. Never write `RX_FRAME`/`TX_FRAME` while the peer owns
    it. `MAX_FRAME` is 1536 bytes; every copy is clamped to it.
17. **Bind the mailbox to one server task.** `OP_TX` and RX doorbells use the
    `server` task ID recorded by `OP_ATTACH`. A re-attach may refresh that ID
    but must not map the same physical window twice.
18. **Use the NIC MAC from `OP_ATTACHED`, never the QEMU default.** The network
    server replaces its initial hardware address before sending; otherwise ARP
    advertises the wrong source. Resolve both network tasks by program ID, not
    by fixed task numbers.
19. **Page tables are registry-owned, not address-owned.** A registered table
    carries `(frame, generation, owner)` via `table_registry::TableToken`; only
    the registry issues tokens. A raw descriptor is never trusted: a walk first
    proves that the target frame is live and that its owner is the space being
    walked. A recycled frame owned by another space is a hard refusal, never a
    free. `retire_space` validates the root and reserves the result before it
    removes any record, so an OOM refusal leaves the registry untouched.
20. **Reclamation is fail-closed.** User unmaps are all-or-nothing; if a range
    cannot be fully proven mapped, the caller leaks its backing run instead of
    freeing frames with live PTEs (`MMAP_LEAKED_FRAMES`). DMA frames are freed
    only after every live address space was scanned and `safe_to_free` is true;
    any doubt leaks (`DMA_LEAKED_FRAMES`). A leak is always preferable to
    handing live memory to a new owner. `unmap_user_pages_at` returning `true`
    means "range unmapped", not "all page-table frames returned".
21. **`safe_to_free` is a CPU-mapping result, not device quiescence.** Without
    an IOMMU or a device-reset proof, a device can still write to a range after
    every PTE is gone. Automatic DMA reclamation is valid only as a
    precondition of the current model: the owning driver stops/reset its queue
    and observes completion before exiting. A shared-DMA physical address is a
    bearer capability; the IPC protocol that distributes it is responsible for
    granting it. Cache maintenance for shared DMA works only while all
    participants use Normal Non-Cacheable aliases after the initial clean.
22. **`GICR_IGROUPR0` is not a usable GICv3 gate on Rain.** It reads `0` for
    timer PPI 18 even when the interrupt is delivered as Group 1 NS. Do not
    reintroduce it as a fail-closed check. The usable gate is
    `GICR_ISENABLER0` + `GICR_IGRPMODR0` + `GICD_CTLR.ARE_NS`/
    `EnableGrp1NS`; an actual timer tick is the delivery proof. Keep the
    group register in diagnostics only.

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
(`from`, `to`, next SP, next resume PC). `silver_handle_task_fault` prints the
fault registers and then dumps that ring before killing the task:

```text
Task <id> killed: Page Fault at 0x<far> (ESR: 0x<esr>)
ELR_EL1=<elr>
SWITCH TRACE from,to,sp,elr:
<from>-><to> sp=<next sp> elr=<resume pc>
```

It costs no output during normal operation; keep it until task lifecycle and
isolation are more battle-tested. The handler only pokes PL011 registers: it
never allocates and never takes a lock, because the dying task may still own
either. Not emitted today, although earlier revisions had them: the `EC` field
(`esr >> 26`) and the eight-word SP_EL0 stack walk that stops at the first
unmapped word.

QEMU exception tracing is invaluable for resume/fault bugs:

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic \
  -kernel target/aarch64-unknown-none/debug/silver_microkernel \
  -d int -D qemu-int.log
```

## Known tech debt

- The loader supports only static `ET_EXEC`, a contiguous 2 MB image window
  and page-uniform permissions; there is no PIE, dynamic linking, demand
  loading or sparse multi-window image layout. `run <path>` loads a whole file
  into kernel memory before parsing it (bounded by `EXEC_FILE_LIMIT`), and the
  dynamic task's argument is always `0`.
- Programs come either from ELFs embedded at kernel build time or from the
  kernel-owned SFS (`/sfs/apps`) via `run <path>`; bundles can carry manifest
  write scopes, quotas, SHA-256 and Ed25519 signatures. There is still no
  per-program credential model beyond capabilities, and a signed build requires
  every filesystem bundle to be signed. Any task that can call `SYS_SPAWN` can
  still request an embedded shell ELF, and `run` still loads any bundle the
  caller can name (subject to its manifest policy).
- Mutex and condvar wait queues still have no growth limits. Mailboxes are
  bounded at 16 messages, but nameserver/control replies also obey backpressure;
  a full client mailbox can therefore block the nameserver. Selective receive
  on a full mailbox can likewise deadlock with its expected sender. Spawn counts,
  kernel heap/VFS growth and per-task memory consumption need quotas for DoS
  resistance. Bounded today: task count, per-task mmap bytes, per-task heap,
  the shared VFS budget, the global user-frame reserve and the per-task frame
  ledger (which includes DMA). Still open: wait-queue growth and kernel-heap
  growth accounting.
- The user heap is limited to one 2 MB L3 window. The allocator supports only
  alignments up to 16 bytes and has no explicit C-style `realloc`/`calloc`,
  thread safety or double-free detection. `SYS_MMAP`/`SYS_MUNMAP` exist, but
  only for anonymous, fixed-placement private pages; there is no file-backed
  `mmap` and the user stack remains fixed at 8 KB. The private window is one
  page. Anonymous mappings are now capped at 16 MiB per task
  (`MAX_MMAP_BYTES_PER_TASK`); this bounds a single task, not the aggregate.
- DMA quiescence is a driver contract, not a kernel guarantee. `safe_to_free`
  proves only that CPU mappings in the MMIO window are gone; there is no
  IOMMU, no device reset on task death and no proof that an armed queue has
  stopped. Revoked runs are quarantined for ~80 ms before reuse, which bounds
  the window but does not make reclamation safe for a device that keeps
  writing. A shared-DMA physical address is a bearer capability, and cache
  coherence for shared DMA relies on all participants using Normal
  Non-Cacheable aliases after the initial clean.
- Interactive mode prevents ordinary background/foreground output interleaving,
  but `demo-tasks` intentionally shares UART. Kernel diagnostics bypass ownership,
  and an IRQ may schedule another task during a FIFO wait. UART RX is interrupt-
  driven but has no escape-sequence parser and drops new bytes when its 256-byte
  buffer is full. Background output is discarded. There are no process groups,
  `jobs` command or general signal API.
- `#[alloc_error_handler]` is unstable on stable Rust. The kernel converts a
  null from its `GlobalAlloc` into a panic; the demo userspace wrapper instead
  prints the layout and exits only the current EL0 task.
- The block driver's `INIT_NOT_FOUND` stage (1) is overloaded: `find_virtio_blk()`
  returns `None` both when the ECAM scan matches no device and when any `?`
  fails *after* a match (capability parsing, `assign_bar4`, `map_phys`). The
  2026-10-27 BAR0 incident showed how expensive that ambiguity is — stage 1
  pointed at "no device on the bus" while the device had been found and
  dropped. Split the stage into "scan found nothing" and "device found, setup
  failed" (the client report then names the exact step).
- Networking is IPv4-only with a static `10.0.2.15/24` address and gateway
  `10.0.2.2`. `net_server` polls every 10 ms; one frame fits per direction;
  there are no virtio-net interrupts, TCP/UDP services or DHCP/DNS. The shell
  `ping` is single-flight ICMP only (no `net`/TCP command surface yet). The
  driver assigns its 64 KiB BAR at a fixed `0x1020_0000` on the assumption that
  the block driver keeps `0x1010_0000`. There is no detach/unmap protocol, so a
  dead `net_server` leaves the driver holding the old mapping until the next
  attach. TX-ring exhaustion drops the frame (`NET DRIVER: TX FRAME DROPPED`)
  rather than blocking the server.
- The compositor still has two presentation paths. SilverHome is zero-copy:
  it owns a kernel surface and `ramfb` scans that buffer. Installed apps use
  the legacy command stream and a full-frame blit; the compositor's own
  engineer menu is a fallback when no client is attached. The `fog-display`
  build adds a third backend: the bootloader's `cont_splash` panel, mapped via
  `SYS_DISPLAY_MAP`, where client-surface attach is impossible and the command
  stream is used. There is no touch driver yet, no DSI/GPU handoff, and the
  panel mode is a configured 720x1650/stride-720 assumption. Present fences are
  best-effort `try_send` (no kernel fence object, no wait), and there is no
  `SYS_SURFACE_DESTROY`: a surface lives until its owner exits. The input
  driver still polls the virtio event queue at ~100 Hz and assumes QEMU's
  fixed `0..32767` absolute range. The input driver's 64 KiB BAR is fixed at
  `0x1030_0000`, extending the fixed-BAR assumption used by the block
  (`0x1010_0000`) and net (`0x1020_0000`) drivers.
- Bundle signing keys are provisioned manually at build time
  (`keys/bundle_ed25519.pub` or `SILVER_BUNDLE_PUBKEY`). There is no key
  rotation, revocation, per-publisher keyring or developer-mode exception,
  and the seeded in-kernel manifests are unsigned (the default build has no
  trusted key). A signed build requires signing every filesystem bundle
  through `scripts/sign_bundle.py`.
- A surface physical address is only returned to a grantee that holds the
  `SCANOUT` right, but a physical address is still a bearer capability: a
  task with `CAP_MMIO` that learns one can map it through `SYS_MAP_PHYS`.
  There is no IOMMU. Scanout frames are pinned/leaked on holder death, so the
  display cannot cause a use-after-free, but the frames are not reclaimed.
- The EL1/kernel exception path now halts (`silver_handle_exception` dumps the
  registers and spins in a `wfe` loop) instead of resuming the current task;
  see the Phase 7 section. The remaining gap is that the halt is a bare spin —
  it does not reset the machine or turn the fault into a debuggable
  core/register dump for offline triage.


## Kernel hardening queue (2026-11-05)

- **Caches on physical hardware:** QEMU does not model caches, so the
  `dc cvac`/`dc ivac`/`tlbi` ordering, the reported `CTR_EL0` line size, the
  AP cold-cache assumption and the AP stack clean are HIL items; re-verify
  them on `rain` before trusting the cached SMP path there (step 8 below).
- **VM extensions:** file-backed and sparse multi-window mappings, growable
  stacks and demand paging on top of `SYS_MMAP`/`SYS_MUNMAP`.
- **Spawn credentials and a userspace DMA-lease syscall** need an explicit
  syscall-scope decision before they can land; current fixes are kernel-only.

## Next steps (by priority)

1. **Kernel fence object.** Replace best-effort `try_send` fence acks with a
   kernel object the client can wait on (sequence + completion + timeout), so
   a frame is provably on screen before the back buffer is reused. This is the
   remaining correctness gap in the zero-copy present path.
2. **Surface lifecycle and app zero-copy.** Add `SYS_SURFACE_DESTROY` and a
   reusable surface pool, then move Calc/Gallery/Notes from the command stream
   to client-owned surfaces (they currently need `CAP_SURFACE` and a direct
   renderer). Keep the compositor's framebuffer path as the fallback.
3. **Bundle key management.** Add key rotation/revocation, a per-publisher
   keyring and an explicit developer-mode policy. The current build-time single
   key is enough for signature verification, not for operating a store.
4. **Hardware verification (rain/fog).** Run the now-enabled boot checks on
   real hardware: multi-group DMA window, DMA-zero/cache maintenance,
   `CAP_SURFACE` mapping and scanout pinning. QEMU does not model caches; these
   are HIL items. Also verify the 16 MiB per-task MMIO/DMA window on the panel
   path (fog UI itself does not start userspace).
5. **SFS durability.** Per-bundle block/inode quotas and atomic replacement are
   done; a metadata journal, crash recovery beyond the current fail-closed
   fsck direction, file-size quotas and larger-image sparse reads remain.
6. **Resource limits.** Wait-queue growth and kernel-heap accounting are still
   unbounded; everything else has an explicit cap.
7. **Network services.** TCP/UDP on smoltcp, DHCP/DNS, virtio-net interrupts
   and more than one frame per direction.
8. **Program credentials and dynamic loader.** Decide which embedded programs
   a task may request, then add argv/`char*`, streaming segments and PIE/dynamic
   linking. This should gate both `spawn` and `run`.

Nameserver watch notifications are explicitly deferred. Later IPC control-plane
work should also address nonblocking/priority replies, send timeouts/cancellation
and the full-mailbox selective-receive limitation recorded above.

## Current verification commands (2026-11-07)

Host logic and crypto:

```bash
cargo test --lib                     # 218 tests
```

Required build profiles (all must stay green):

```bash
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,smp-selftest
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,tablet
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-reloc
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-auto-reset
cargo build --release --target aarch64-unknown-none --no-default-features --features no_std,fog,fog-hw-selftests
```

QEMU smoke (console product): boot with `-display none -serial stdio`, require
`howl ready`, then send `help`, `uname`, `ls /`, `health` over the serial line;
`health` must report `dma leaked 0`. Run `run /sfs/apps/scopeprobe.elf`
(status 0) and finish with `scripts/sfs_fsck.py`. There is no framebuffer,
surface, compositor or input device to test: those were removed on 2026-11-08.
The SMP run (`-smp 4`) expects 15 PASS / 0 FAIL.

Bundle signing check (needs a throwaway key):

```bash
scripts/sign_bundle.py keygen /tmp/werewolf-dev
SILVER_BUNDLE_PUBKEY=$(cat /tmp/werewolf-dev.pub) ./build_and_qemu.sh --no-run
# boot with a fresh image: unsigned probe must fail,
# append `echo sig=<signature> >> /sfs/apps/scopeprobe.manifest` and it runs.
```

The four MMU/DMA boot self-tests are now unconditional. A boot that stops at
`EL1 EXCEPTION kind=9` before `MMU ASID OK` is a real failure in the
TLB/ASID/heap/DMA-zero invariant, not a skipped test.

## Workflow for a new feature

1. Code → `cargo fmt --all` and `cargo fmt --manifest-path userspace/Cargo.toml`
2. `cargo build` (host — catches silly mistakes faster)
3. `cargo build --target aarch64-unknown-none --no-default-features --features no_std`
4. Also check `--release`: optimized userspace exposes register-preservation
   bugs that debug builds can hide. For regressions, separately build with
   `--features no_std,demo-tasks`; this replaces the interactive boot.
5. For the default profile, verify `ls`, `echo 'hello' > test.txt`,
   `cat test.txt`, `mkdir docs`, quoted echo, `cd /bin`, `pwd`, relative paths,
   `cd ..`, `ps`, and `spawn`/`kill`/`wait`. Also check fastfetch and the logo,
   confirm `ping 8.8.8.8` prints `PING 8.8.8.8: OK`, and make sure `ps`
   lists live `net_driver` and `net_server` tasks.
   Run `bg demo 12`, confirm shell output stays free of `AWAKE`, then `fg 3`.
   Send Ctrl-Z, verify `ps` = stopped and silence while stopped, resume with
   `fg 3`, stop again and verify kill/wait. Repeat with blocked `demo 7` and
   verify a notification received while stopped is delivered after resume.
   For demo-tasks, run 5–30 seconds,
   repeat timing-sensitive runs and expect exactly two intentional `EL0 SYNC
   FAULT!` blocks from tasks 15 and 16, plus
   `COPYOUT READONLY REJECT=PASS`, `COPYOUT UNMAPPED REJECT=PASS`,
   `LIFECYCLE EXIT/UNREGISTER=PASS`, `LIFECYCLE MUTEX CLEANUP=PASS`,
   `HEAP GROW/SHRINK=PASS`, `MALLOC FREE/REUSE=PASS`,
   `MALLOC BRK TRIM=PASS`, `GLOBAL ALLOC BOX=42 VEC_LEN=32 PASS`,
   `GLOBAL ALLOC TRIM=PASS`, `IPC TIMEOUT=PASS`,
   `IPC BP SENDER RESUMED=PASS`, `IPC BP BACKPRESSURE=PASS`,
   `IPC EXIT WAKE=PASS`, `SECURITY IPC SENDER=PASS`,
   `SECURITY MUTEX NONOWNER=PASS`, `SECURITY MUTEX OWNER=PASS` and
   `SECURITY SLEEP OVERFLOW=PASS`.
6. Update the README checklist and this file.

## 2026-11-08 — boot-stack root cause, 6 CPUs, SFS stage 1 (commits 4f0f5e3, 0bd0347, 85f90b2)

The "layout-flaky boot panic" was **not** miscompilation: `kernel_main`
overflowed the 16 KiB boot stack by ≥1 KiB before `sfs::load`, and the
overwrite hit whatever static sat at the `.bss` tail (`DMA_QUARANTINE`
slots in the repro) — so unrelated code changes flipped the panic on and
off. Proven with a gdb hardware watchpoint (`memset` from
`console::write_dec_u64`, SP 1 KiB below `__stack_bottom`). Fix: the boot
stack is 64 KiB (`linker.ld`). The O3 `b .` self-loop findings need
re-examination in this light.

With the fix: `MAX_CPUS=8`, QEMU `-smp 6` fully green (churn and
push-migration included). SFS stage 1: format logic extracted to
`sfs_format.rs` (host-testable, `SfsDisk` trait), fsck at mount
(fail-closed on cross-links/dangling entries, leaks tolerated), in-place
positioned overwrites, rename rollback, `virtio_blk::sector_count()`,
adaptive image load up to 8 MiB, `scripts/make_sfs.py` formatter.
200/200 host tests; `-smp 2/4/6` boots with 1 MiB and 8 MiB disks; fsck
clean; shell write/append/rename/mkdir/rm/rmdir smoke PASS.
Next: fog/Redmi 10C boot PoC (see SMP_STATUS and the T20.1 review), then
drivers: GCC/pinctrl → GENI I2C → touch.

## 2026-11-10 — GTK3 loads and runs in the guest (Phosh prerequisite)

Milestone: the real GTK3 3.24.52 stack now gets through `ld.so`, loads every
library, starts a `GMainLoop`, creates the Wayland window and only stops when
it wants to rasterise its first icon. Before this session the guest died at
`libz.so.1: failed to map segment from shared object`.

### What was blocking and what changed

- **File-backed `MAP_PRIVATE` is now paged, not contiguous.** The old path
  read the whole file into a kernel `Vec` (capped by `MAX_FILE_BYTES` = 8 MiB;
  `libgtk-3.so` is 8.5 MiB) and asked the frame allocator for one contiguous
  run (up to 2 000 pages). After a few hundred mappings that run no longer
  exists. `linux_mmap` now calls `scheduler::mmap_file_paged`, which allocates
  one frame per page, fills it through a `pread` closure and publishes it with
  `mmu::map_user_page_at`. `MmapRegion` gained a `frames: Vec<(phys, gen)>`
  for these regions; `munmap`, `mprotect` splits, `fork` (deep copy) and
  teardown all handle it. The 8 MiB whole-file cap is gone for mmap; the new
  bound is 64 MiB per call, still checked against the per-task quota.
- **Limits for a toolkit process.** `MAX_MMAP_REGIONS` 128 → 1024 (GTK maps
  hundreds of segments), `MAX_MMAP_BYTES_PER_TASK` 24 → 160 MiB,
  `TASK_LIMIT_DIVISOR` 4 → 2, QEMU DRAM 128 → 256 MiB (health now reports
  64 224 frames). `build_and_qemu.sh` already passed `-m 256M`; the kernel
  just refused to use it.
- **`newfstatat` classified directories as files.** SFS reports a directory
  inode's size as 0, so `file_len` succeeded and `st_mode` said `S_IFREG`.
  xkbcommon stat'ed its own config root, saw a file, and refused every include
  path. Directories are now probed first (`read_dir`).
- **`vfs::open` accepted paths that do not exist**, storing an `OpenFile` and
  failing the first read with `EBADF`. xkbcommon probes `rules/evdev.pre`
  (absent in xkeyboard-config 2.49), got a bogus fd and reported "Bad file
  descriptor". `open` now probes `len()` and returns `ENOENT`.
- **`scripts/sfs_inject.py` can write into subdirectories**: `mkdir`,
  `write_file_at("xkb/rules/evdev", …)`; plain `write_file` is unchanged.

### Verified

- `cargo test --lib`: 221/221.
- Kernel build, QEMU boot on 256 MiB, `health` 64 224 frames, Linux series
  still green (hello/rw/chain/fork/pipe/ipc/signal/libc/threads/unixsock/
  exitgroup/asyncsig/dyn/pthread/groupsig), `wget https://example.com` → 200.
- Guest GTK log now ends at:
  `Could not load a pixbuf … window-minimize-symbolic.symbolic.png` →
  `Gtk:ERROR … image-missing.png: Unrecognized image file format` →
  `Bail out!` (SIGABRT, exit 134). Everything before that is clean.

### Temporary GTK image (lives in /tmp, not in the repo)

`/tmp/opencode/gtk/disk_gtk.img` — 256 MiB, copy of `disk.img` resized with
`scripts/sfs_resize.py 256 4096`, then:
- 67 GTK/GLib/GIO libraries plus `ld-linux-aarch64.so.1`, `libc.so.6`,
  `libm.so.6` at `/sfs` (set built from `LD_DEBUG=libs` "calling init" lines;
  host root `/tmp/opencode/gtk/root`, Arch ARM packages);
- `gtkdemo` (interpreter `/sfs/ld-linux-aarch64.so.1`), `gtk-serve`
  (`-DSERVE_ONLY` build of `tests/linux/wayland_real.c`);
- `/sfs/xkb/**` (298 files from Arch `xkeyboard-config` 2.49, extracted from
  the package's `xkeyboard-config-2` tree; `XKB_CONFIG_ROOT=/sfs/xkb`);
- `/sfs/etc/passwd`, `/sfs/etc/group`, `/sfs/gschemas.compiled`,
  `/sfs/DejaVuSans.ttf`, `/sfs/fonts.conf` (`cachedir /tmp/fontcache`);
- env from the harness: `HOME=/tmp`, `XDG_RUNTIME_DIR=/tmp`,
  `WAYLAND_DISPLAY=wayland-test`, `GDK_BACKEND=wayland`, `LD_LIBRARY_PATH=/sfs`,
  `FONTCONFIG_FILE=/sfs/fonts.conf`, `GSETTINGS_SCHEMA_DIR=/sfs`.

Repro: back up `disk.img`, copy the GTK image over it, boot
`./build_and_qemu.sh --no-viewer`, run `linuxrun-gtk-serve`. Restore
`disk.img` with `git checkout -- disk.img` afterwards.

### Next (in order)

1. **gdk-pixbuf image loader.** `gdk-pixbuf-query-loaders` from the installed
   2.44.8 package prints an empty loader list (only a missing
   `libopenraw_pixbuf.so` warning); Arch's package ships loaders as modules
   and/or glycin. Need a PNG loader for GTK's GResource icons: ship
   `libpixbufloader-png.so` + a `loaders.cache` and set
   `GDK_PIXBUF_MODULE_FILE`/`GDK_PIXBUF_MODULEDIR`, or build gdk-pixbuf with
   built-in PNG. Quick proof-of-window workaround: make the demo window
   undecorated (`gtk_window_set_decorated(FALSE)`) so no CSD icon is loaded,
   but real Phosh apps need the loader.
2. **fontconfig writes.** `faccessat` still returns `EROFS` for every `W_OK`
   check, including `/tmp`, so fontconfig cannot create `/tmp/fontcache`.
   Allow `W_OK` under `LINUX_SCRATCH_PREFIX`.
3. **Post-abort faults.** After gtkdemo aborts, two helper tasks (9, 11) take
   an EL0 page fault inside library code (e.g. `0x418D94EC`) before gtk-serve
   exits 1. Understand whether these are GTK threads killed by `exit_group` or
   a real teardown bug; re-check once the icon abort is gone.
4. Then: Phosh. The mini compositor still lacks the wlroots protocol set Phosh
   needs (`wlr-layer-shell`, `wlr-screencopy`, `wlr-foreign-toplevel`,
   `wlr-output-management`, idle-inhibit, gamma-control, ...), and Phosh is
   GTK3 with gnome-session/upower/gsettings expectations. GTK4 + libadwaita
   follow.

### GTK3 frame pipeline closed (same day, follow-up)

The window is not just created: the compositor now receives and verifies a
real shm frame from GTK. Guest output is

```text
gtk window shown
gtk-serve: shm frame committed
gtk mainloop ok
gtk client ok
```

Compositor/test fixes that were needed:

- **Input buffer was 1 KiB.** GTK's registry/shm/configure batches overflowed
  it, the tail was dropped and the stream desynchronised, so
  `wl_surface.commit` was never seen. `incoming` is now 64 KiB and a
  truncated tail is carried into the next `recvmsg` (`incoming_pending`).
- **Surface roles were assigned by creation order.** GTK creates auxiliary
  surfaces before the toplevel; the real toplevel ended up in
  `subsurface_surface_id` and its commit was swallowed by the subsurface
  branch. A `surface_ids[]` registry tracks every `wl_surface`, every commit
  goes through one handler, and `xdg_wm_base.get_xdg_surface` names the real
  toplevel.
- **Frame callbacks were never completed.** `wl_surface.frame` now gets an
  immediate `wl_callback.done`; this is what made GTK paint.
- **Keymap.** The compositor sends `NO_KEYMAP` (format 0, size 0) instead of
  the `WEREWOLF_KEYMAP` marker; `wlclient` accepts both and the GTK
  xkbcommon warning is gone. A real keymap comes with keyboard support.
- **fontconfig cache.** `faccessat(W_OK)` is allowed under `/tmp`, and
  `gtk-serve` creates `/tmp/fontcache` before exec; the "no writable cache
  directories" warning is gone.

Do **not** release the attached `wl_buffer` immediately after commit: GTK
still owns the buffer through its staging cairo surface and aborts with a
cairo reference-count assertion. A real compositor must release after
compositing (or double-buffer), not inline in the commit handler.

`linuxrun /sfs/linux-wlreal` (two clients + clipboard) still passes after
these changes, and the updated 256 MiB image is saved at
`/home/ivan/phosh-state/disk_gtk3.img`.

### Where Phosh is blocked now

1. **Image loaders.** Arch's gdk-pixbuf 2.44.8 links `libglycin-2` and ships
   no built-in PNG loader (`gdk-pixbuf-query-loaders` lists nothing). The
   demo is undecorated so it never asks for a CSD icon. Real apps and Phosh
   need either glycin's loaders (`/usr/lib/glycin-loaders/2+/glycin-image-rs`
   + config) or a gdk-pixbuf PNG module.
2. **Display sink.** The mini compositor verifies frames but presents
   nothing. A real compositor must composite into the ivshmem surface (or
   another framebuffer) so the host viewer shows the desktop.
3. **wlroots protocol set** for Phosh: layer-shell, screencopy,
   foreign-toplevel, output-management, idle-inhibit, gamma-control, etc.

### Boot flakiness fixed: `print_logo`'s iterator state

The intermittent pre-prompt shell fault (~1 in 3 boots on the 64 MiB image)
was finally caught with a saved-x30 print. The caller was `fastfetch`, and
the faulting `trim_matches` received `x0=0x4000F416`, `x1=0xFFFFFFFFFFFF8DB1`.
`print_logo`'s `LOGO.lines().position(...).filter(...).skip(...).take(...)`
chain had its `start` field spilled to an **unwritten stack slot**
(`fastfetch+0x134: ldr x9, [sp,#200]`, no store anywhere in the frame);
on some boots that slot held a stale pointer, and `Lines::next` computed
`haystack + start` from it. `print_logo` is now a plain byte scan with no
`Lines`/`trim` state. **10/10 clean boots** afterwards, up from ~2/3.

### Glycin image loader: shipped, but the D-Bus handshake hangs

`gdk-pixbuf` 2.44.8 is glycin-only on Arch. The loader was shipped at
`/sfs/usr/lib/glycin-loaders/2+/glycin-image-rs` (interpreter patched to
`/sfs/ld-linux-aarch64.so.1`), its config rewritten to that path under
`/sfs/share/glycin-loaders/2+/conf.d`, and the launcher exports
`GLYCIN_DATA_DIR=/sfs/share` + `GLYCIN_DISABLE_SANDBOX=1`. `glycintest`
now finds the loader directory and **spawns the loader task**, which exits
0, but the parent stays blocked: glycin passes a private GDBus connection
via `--dbus-fd` and waits for the first D-Bus message. The next probe is
`SO_PEERCRED`/socketpair behaviour in the Linux socket layer; until that
works, GTK windows must stay undecorated (no CSD icons) and real apps
cannot load PNG icons.

### Next: present the frame

The compositor still verifies shm frames instead of displaying them. The
next architectural step is a display sink: either a `/dev/ivshmem`-style
mmap for Linux compositors, or a native presenter that receives the
composited surface. Only after that does Phosh's layer-shell work become
visible.

### Display sink: `/dev/ivshmem` and a WIN1 blit (step 1 of the Phosh track)

The compositor no longer verifies frames into the void. Linux tasks can now
open `/dev/ivshmem`; `mmap(MAP_SHARED)` maps the QEMU BAR Non-Cacheable and
`gtk-serve` blits each committed shm frame into the host-shared surface using
the native Wolfser header (`WIN1`, width/height/stride at 0x08..0x10, pixels
at 0x1000, `window_request` at 0x2C).

- `src/ivshmem.rs`: finds the ivshmem PCI function through ECAM, enables
  memory space, sizes BAR2 (offset 0x18, 64-bit), programs it at
  `0x1040_0000` (the same window Wolfser uses) and exposes `ensure()`.
- `linux_fd`: `Target::Ivshmem`, `/dev/ivshmem`, `is_ivshmem`.
- `scheduler::map_ivshmem` maps the BAR into the current Linux task through
  the MMIO window (`mmu::map_phys_nc_to_user`), no capability required.
- `linux_mmap`: `MAP_SHARED` on that device maps offset 0 / up to 4 MiB.
- The compositor writes the header, swizzles Wayland ARGB8888 to the viewer's
  RGBA8888, drops XRGB alpha, and clips to `(4 MiB - 4 KiB)` so a
  full-BAR 1024x1024 frame cannot run past the mapping.
- The output is logical 512x384 at `wl_output.scale=2`, i.e. a 1024x768
  buffer, which fits one BAR with the header.
- `HEAP_PAGES` 4096 -> 8192 (32 MiB): a GTK 1024x768 window creates a ~12 MiB
  shm pool, which must not abort the kernel heap allocator.

Proof: `QT_QPA_PLATFORM=offscreen build/window-viewer/werewolf-window
--shm /dev/shm/werewolf-window --snapshot /tmp/opencode/gtk_orig.png
--timeout 120` while the guest runs `linuxrun-gtk-serve` prints

```text
snapshot: 1024x768 frame=2 nonwhite-samples=126
```

and the PNG is the real GTK window ("WerewolfOS GTK demo" centred on the GTK
background). Host viewer, compositor and client all agree on the frame.

Known issue found while validating: `linuxrun /sfs/linux-memfd` faults right
after `fork()` when a `MAP_SHARED` memfd is mapped. **HEAD reproduces it**
(worktree build), so it is a pre-existing regression from the earlier
256 MiB/mmap work, not this slice. The fault is EC=0 / FAR=0 in libc after
fork; the memfd fd/pread/pwrite steps before fork pass. Track it separately.

### glibc fork fixed; glycin now stops at the D-Bus fd (next)

`linuxrun /sfs/glibcfork` (a minimal dynamically linked glibc program) used to
fault in both parent and child immediately after `fork`. Three real bugs:

1. **Paged file-backed regions were mapped at physical 0 in the child.**
   `fork` deep-copies `MmapRegion.frames` (libc's text/data are paged file
   mappings), but the install loop still passed `region.physical_start` — which
   is 0 for that variant — to `map_shared_user_pages_at`. The child's first
   instruction after `svc` fetched from physical 0. The loop now maps each
   frame with `map_user_page_at`.
2. **The parent was made read-only for COW over those same paged regions.**
   The child already has a private copy, so the parent must stay writable.
3. **`cow_fault` treated a paged region as one contiguous run** (again
   `physical_start = 0`) if a write fault did happen. It now copies and
   replaces the single frame.

Also implemented `CLONE_CHILD_SETTID` / `CLONE_CHILD_CLEARTID` for plain
`fork`: glibc's `_Fork` passes the child TID address in the TCB; without the
write the child keeps a stale TID. `scheduler::set_child_tid` breaks the page
COW in the child, switches to the child's TTBR0, validates and writes the u32,
then restores the parent's space.

Result:

```text
before fork
after fork p=0   (child)
after fork p=8   (parent)
wait done status=0
```

**Glycin:** with fork fixed, `glycin-image-rs` starts and logs
`Loader glycin-image-rs v2.2.4 startup`, then panics with

```text
Failed to create private DBus connection:
InputOutput(Os { code: 9, kind: Uncategorized, message: "Bad file descriptor" })
```

The loader is given the D-Bus fd via `--dbus-fd 3`; zbus sees EBADF. Next
probe: a tiny `socketpair` + `dup2(fd, 3)` + `execve` test that prints
`fcntl(3, F_GETFD)` / `getsockopt(3, SO_TYPE)`, to decide whether the fd is
lost across our `execve` or whether `dup2`/`getsockopt` is at fault. The loader
also needs `RUNPATH=/sfs:$ORIGIN` (injected) because glycin spawns it with a
sanitized environment; that part is solved.

`linux-memfd` was failing because of the same glibc-fork bugs; it should be
retested now.

**Step 2 status:** loader reaches its own startup, D-Bus fd is the only
remaining blocker. **Step 3 (wlr-layer-shell/Phosh) not started.**

### Step 2: glycin reaches the D-Bus layer; the kernel gaps it exposed

The loader now starts, receives its `--dbus-fd`, and completes the zbus
handshake (`D-Bus connection to glycin created`). Getting there required
fixing eight real Linux-ABI gaps, each found by reproducing it in a small
program:

1. **`FD_CLOEXEC` is now modelled per descriptor.** `fcntl(F_GETFD)` reports
   it, `F_SETFD` sets it, `F_DUPFD_CLOEXEC` duplicates with it, `pipe2` and
   `socketpair` honour `O_CLOEXEC`/`SOCK_CLOEXEC`, and a successful `execve`
   closes every marked descriptor. Without this, Rust's `Command::spawn`
   blocked forever reading the child end of its exec-error socketpair:
   the child never closed it, so the parent never saw EOF. This was the
   single biggest cause of the earlier "Bad file descriptor" and hangs.
2. **`SOCK_SEQPACKET` socketpairs** are accepted (kept as byte streams).
   That is the type Rust uses for the exec-error channel.
3. **`ioctl(FIONBIO)`** toggles non-blocking on sockets/pipes. Rust's
   `set_nonblocking` uses it; without it glycin's socket setup got `EBADF`.
4. **`epoll_pwait` accepts large `maxevents`** (glibc asks for 1024; the old
   cap of 64 returned `EINVAL` and glycin's async runtime spun).
5. **`SO_PEERCRED` reports a non-zero pid.** zbus reads peer credentials
   during negotiation; pid 0 made glycin discard the connection object with
   an `io::Error` of errno 0 (`Success`).
6. **Lazy `PROT_NONE` reservations.** glibc's `new_heap` maps
   `2 * 64 MiB` PROT_NONE/MAP_NORESERVE and expects success; eager frame
   allocation answered `ENOMEM` and glibc retried the same unaligned 64 MiB
   mapping forever. Anonymous mappings with `prot == 0` are now tracked
   lazily (VA only, no frames, no charge) and backed on first touch after
   `mprotect`/`MAP_FIXED`. All other anonymous mappings stay eager.
7. **`fork()` from a thread.** The child is a copy of the *process*, so
   `fork_current` snapshots the group leader (image, heap, mmaps) while
   keeping the caller's resume frame and TLS; `wait_child` matches on the
   thread-group id. Lazy pages the leader had already touched (a pthread TCB
   and stack) are deep-copied into the child; handing it an empty
   reservation made its first libc call dereference a null TCB field.
8. **`mmap` for `MAP_FIXED` over lazy ranges**, `munmap` and `mprotect` use
   lenient page-table walks that skip holes instead of failing.

Glycin also needs the *exact* opt-out value
`GLYCIN_DISABLE_SANDBOX=i-know-the-risks`; with `1` it still chose bwrap and
failed to spawn. With the correct value it spawns the loader directly and
passes `--dbus-fd` successfully.

`linux-memfd` (broken before) and the new `linux-threadfork` test pass, as do
`linux-sockets`, `linux-pthread` and the two-client Wayland test.

**Remaining in step 2:** after the zbus connection is up, glycin's
`gly_loader_load` still does not complete within its 60 s limit and reports
no result. The loader logs `D-Bus connection to glycin created`; the next
probe is the instruction/protocol exchange that follows the handshake
(who sends the first method call, and whether its reply wakes the waiting
main context). The loader's stderr is piped by glycin, so debugging uses a
wrapper that relays it into the ivshmem BAR at `0x3F0000`.

### `EPOLLONESHOT` and the exact place glycin stops now

`epoll` ignored `EPOLLONESHOT`. async-io/polling registers every source with
`EPOLLIN|EPOLLOUT|EPOLLONESHOT` and re-arms only the directions that still
have waiters. Because the flag was ignored, an always-writable eventfd was
reported forever: the reactor spun re-arming instead of servicing the real
socket. Entries now disarm after one report and re-arm on `EPOLL_CTL_MOD`.

With that fixed the socket exchange is visible and correct up to one point:

```text
glycin  -> loader  276 bytes + 1 fd   (Init; the image socket travels in SCM_RIGHTS)
loader  -> glycin   16 + 260 bytes    (receives them, reads the image)
loader  -> glycin  240 bytes          (Init reply)
```

The loader's reply is never read. glycin's async-io reactor (`epoll` fd 3)
had fd 9 registered with `EPOLLIN`, reported the *handshake* message, and
then parked; zbus never issues an `EPOLL_CTL_MOD` for fd 9 again, i.e. the
connection's reader task is not polled after the handshake. The next probe is
on glycin's side of the fence: the `executor.tick()` loop
(`async-global-executor` task) and whether its wakeup reaches the async-io
reactor. The loader's stderr is piped, so the relay wrapper described above
is the way to see its logs (`D-Bus connection to glycin created` is the last
line it prints).

## Step 2 complete: glycin decodes images end to end

`glycintest` now prints, for a real 64x64 PNG:

```text
glycin step 4: image=0x... err=(none)
glycin step 5: frame=0x... err=(none)
glycin ok 64x64
```

That proves the whole chain: glycin_core -> private zbus connection -> loader
spawn -> `Init` with the image fd in SCM_RIGHTS -> memfd frame back to the
caller. Fixes that closed the last gaps (besides the EPOLLONESHOT commit):

- **`fcntl(F_ADD_SEALS)` / `F_GET_SEALS`** on a memfd. glycin seals the frame
  pool (`F_SEAL_SHRINK|F_SEAL_GROW`) and treated our `EINVAL` as
  "Failed to allocate memory: EINVAL".
- **`getrandom(GRND_INSECURE)`** is accepted (zbus generates its GUID with
  it); all three flags return kernel CSPRNG bytes.
- **`sched_getaffinity` reports the real CPU mask** instead of one CPU, so
  Rust's `available_parallelism` matches the 2-vCPU QEMU guest.
- **`ASYNC_GLOBAL_EXECUTOR_THREADS=4`** in the Linux launcher environment.
  With the default pool the zbus `executor.tick()` loop and the connection
  reader starve each other and the `Init` reply is never read; four workers
  make it deterministic. This is a user-space scheduling workaround, not a
  kernel rule.

### Remaining for GTK3 icons: memory corruption in the gdk-pixbuf path

`gdk-pixbuf` does list `image/png` in the guest (`pixformats`: 28 formats) and
`glycin` decodes the same file through its file, stream and bytes APIs
(`glycinsrc`). Loading through `gdk_pixbuf_new_from_file` now reaches glycin
but then hits heap corruption:

```text
malloc(): unaligned tcache chunk detected   (exit 134)
```

and one run died with an instruction abort at a libc address
(`FAR=ELR=0x413594EC`, level-0 translation fault). A direct pixel copy into
a heap buffer (`glycinsrc`, row-by-row `memcpy` from
`gly_frame_get_buf_bytes`) did not complete cleanly either, so the next probe
is the **frame transport**: the memfd `MAP_SHARED` window and the lazy heap
(`new_heap` PROT_NONE -> `mprotect`) are the two places where a page could be
mapped twice or with the wrong contents. The host's gdk-pixbuf loads the same
PNG, so this is our memory layer, not the toolkit.

GTK4/GDK use `GdkTexture` and talk to glycin directly, so GTK3 icons are the
only consumer of this path.

Diagnostics for the corruption (temporary `describe_fault`, now removed) gave
the first concrete clue: the fatal access is a **data abort at a page with no
mapping at all**, at the start of a 16 KiB hole in the library layout:

```text
reg 0x40E39000 n=3   prot=1 (RO)      <- end of one segment
   0x40E3C000        no region, PTE=0 <- first access faults here
reg 0x40E40000 n=575 prot=5 (RX)      <- next segment
```

The layout is the classic ELF order (RX, RO, RW), so the missing pages are a
library's writable data segment that `ld.so` expected at `0x40E3C000`. Other
runs instead report `free(): invalid pointer` or `malloc(): unaligned tcache
chunk detected` in glycin's worker threads. No `mmap` syscall returned an
error in those runs.

Next probe: trace `mmap`/`munmap` (address, length, prot, flags, fd, result)
for the load of the library that owns `0x40E39000` and check whether its RW
segment mapping was ever issued and recorded, or whether a later `munmap` of
a lazy reservation removed it. The fault handler only consults
`mmap_regions`, so the diagnostic should also print `linux_image.runs`.

### MMU: non-global user mappings and ASID reuse

Two TLB correctness fixes while chasing heap corruption:

- Every user leaf descriptor now sets the **nG** bit (bit 11). Without it a
  translation was global, so the per-space flush (`tlbi aside1is`) after a PTE
  edit could not invalidate it.
- `alloc_asid` broadcasts `tlbi vmalle1is` before handing a recycled ASID to
  a new address space. All tasks map the same user VAs (image, heap, stack);
  a stale entry from the previous owner would alias the new space into a
  frame it does not own.

Existing tests (`linux-memfd`, `linux-threadfork`, `linux-futexping`, the
two-client Wayland test, host unit tests) stay green, and glycin still
decodes the 64x64 PNG.

### Still open: silent zeroing of a heap page

A minimal repro remains:

```c
for (round = 0; round < 20000; round++) {
    block = malloc(200 * 1024);
    memset(block, pattern, 200 * 1024);
    verify every 97th byte;
    free(block);
}
```

`chunktest` fails after a few hundred rounds: one byte in the middle of the
block reads 0 (sometimes 0x18, a glibc chunk-header value). Facts gathered:

- single-threaded, so no concurrent malloc user;
- the page's PTE stays stable for the whole run (checked with a debug syscall
  returning the physical frame);
- no user allocation/free path ever reports reusing a heap frame;
- disabling trim and raising the mmap threshold does not help;
- removing the virtio-net device does not help;
- the earlier "freeing heap frame" diagnostic was a **false positive**: the
  owner scan matched the mmap region that was being unmapped.
- `brktest` (same 200 KB block written and verified without malloc) can pass
  2000 rounds, but is not a reliable control.

Next probe: extend the ownership audit to **kernel allocations** (page-table
frames via the table registry, kernel linear-map writes) or run QEMU with a
watchpoint on the physical frame. A shadow allocation bitmap inside the frame
allocator would also catch a double grant directly.

### gdk-pixbuf still refuses PNG

`gdk-pixbuf` lists `image/png` (28 formats) but `gdk_pixbuf_new_from_file`
still says "Couldn't recognize the image file format". `mmap(MAP_PRIVATE)` of
`/sfs/test.png` is byte-identical to `pread` (checked), and glycin decodes the
same file through file/stream/bytes. The next probe is
`gdk_pixbuf_get_file_info` (does the sniff succeed?) and whether the format
entry has a working module: the aarch64 build may expect a
`libpixbufloader-glycin.so`/cache the image does not have, while the host
build registers the glycin formats in code.

Additional fact from the corruption probe: glibc only calls
`madvise(heap, 2 MiB, MADV_DONTDUMP)` (advice 14), which is a dump filter and
correctly a no-op here, so trimming/`MADV_DONTNEED` is not involved. The bad
byte is sometimes an **older round's pattern**, i.e. the frame keeps or
acquires content this process wrote in an earlier pass; that points at an
aliased live mapping rather than a lost store. The kernel allocation audit
did not find a user-side double grant, so page-table frames and other
kernel-side allocations are the remaining suspects, or a device write.

### Heap corruption: the shape of the damage (new)

A stronger probe of `chunktest` (single-threaded `malloc(200 KiB)` /
`memset` / verify / `free`) turned the random single-byte reports into a
precise picture. The probe used three temporary debug syscalls (now removed,
recreate from this note):

- `4243(va)` prints the frame behind a user page, whether the allocator still
  has it, whether it is a registered page table, its generation, the task that
  tracks it, and the byte as read through the kernel linear map;
- `4244(va)` returns that frame;
- `4245(lo, hi)` scans DRAM for runs of the repeated record.

What the probe shows when a round fails:

- The damage is a **suffix of the malloc block**: all bytes from some offset
  `X` to the end of the 200 KiB request are replaced, and every byte before
  `X` still holds the pattern. `X` is 16-byte aligned and varies run to run
  (36896, 152672, 164704, ...).
- The replacement is a **constant 16-byte record `{u64 0x218, u64 0}`**,
  repeated exactly to the block end. `0x18` in earlier reports is simply the
  low byte of `0x218`.
- The glibc chunk header in front of the block is untouched
  (`prev_size = 0`, `size = 204817`).
- Reading the same byte through the **kernel linear map** returns the same
  value (`kbyte = 0`), so this is real memory content, not a stale user-TLB
  view of another frame.
- At that moment the frame is still **allocated** in the allocator bitmap
  (`alloc=1`), is **not** a live page table (`table=0`), and is still tracked
  by the failing task as its own heap data (`owner=<task>`), with the
  generation the task recorded. So it is not a table-frame alias and not a
  frame handed to another task.
- A fresh `memset` over the whole block then **sticks**
  (`rewrite mismatches=0`): the mapping is writable and correct once the bad
  moment has passed.
- The failure reproduces with `-smp 1`, so it is not cross-CPU TLB/migration.
- glibc issues exactly three `brk` calls (two queries and one growth to the
  2 MiB cap) and never shrinks, so trim/regrow is not involved either.

`0x218` (536) is in the same range as the frame generations observed at
detection (662-700), so a `(generation, frame = 0)` pair is a plausible
reading of the record, but that is unconfirmed.

Next probes, in order:

1. **Find the writer** with a QEMU gdb hardware watchpoint while the serial
   console is driven over a unix socket. Watch the last 8 bytes of the block
   (user VA `0x4083_2008`) *and* the kernel linear alias
   `KERNEL_VIRT_BASE + PA + 0x1FF8` of the same frame: a user-VA watchpoint
   alone misses a kernel linear-map writer. Auto-continue in a gdb command
   script and log `$pc` plus the written value; the block suffix makes the
   last bytes a reliable trigger.
2. **Identify `0x218`**: scan DRAM for runs of `{0x218, 0}` outside the block
   (syscall 4245 above) and inspect the surrounding memory. If the record is
   part of a kernel `Vec`, the neighbour bytes identify the structure.
3. If the writer is the kernel, audit every kernel path that writes a user
   frame through the linear map (`zero_page`, `cow_copy_run`, `copy_to_user`
   users) against the frame being live user data.

The earlier "freeing heap frame" diagnostic remains a false positive (the
owner scan matched the mmap region being unmapped).

## 2026-11-11 — Phosh step 3: wlr-layer-shell handshake

Phosh draws its panel, lock screen, OSK and notifications as layer-shell
surfaces, so the mini compositor now speaks
`zwlr_layer_shell_v1` version 4 (the wlroots protocol XML is vendored at
`third_party/wayland-aarch64/protocol/wlr-layer-shell-unstable-v1.xml`;
the client header and private code are generated with `wayland-scanner`).

- **Server** (`tests/linux/wayland_real.c`): advertises global id 17 and
  handles `get_layer_surface`, `set_size`, `set_anchor`,
  `set_exclusive_zone`, `set_margin`, `set_keyboard_interactivity`,
  `get_popup`, `ack_configure`, `destroy` and `set_layer`. The first
  `wl_surface.commit` of the layer surface emits `configure(serial, w, h)`;
  the per-client state (surface id, requested width/height, configured/acked
  flags) is part of `ClientState`, so the save/restore path keeps it across
  the two-client test.
- **Client** (`tests/linux/wlclient.c`, `source` role): binds the global,
  creates a 256x64 `TOP|LEFT` layer surface with a 64 px exclusive zone,
  commits without a buffer, waits for `configure`, acknowledges it and
  destroys the surface. Prints `layer-shell ok`.

`linuxrun /sfs/linux-wlreal` passes: both clients exit 0 and the harness
prints `libwayland 2 clients ok` (the layer-shell line interleaves with the
clipboard output on the shared console).

Not done yet, and needed before a real Phosh session:

1. **Composite** layer surfaces into the WIN1 sink honouring anchors,
   margins, exclusive zones and layer order (the handshake alone does not
   paint them), and allow more than one client/surface in `gtk-serve`.
2. **GTK4 + libadwaita + gtk-layer-shell** in the SFS image. Phosh 0.58 is
   built on GNOME 50 (GTK4) and wlroots 0.20.2; the current image only has
   GTK3, so Phosh itself cannot be installed yet.
3. Remaining wlroots protocols Phosh uses: `wlr-foreign-toplevel-management`,
   `wlr-screencopy`, `wlr-output-management`, `idle-inhibit`,
   `gamma-control`, plus phoc's `phosh-private` role.
4. Input injection from the host viewer (none yet), and the two open memory
   bugs below.

Open bugs unchanged: the `{0x218, 0}` heap-suffix corruption (see above)
and gdk-pixbuf's PNG refusal.

### Layer surfaces now composite into the WIN1 sink

`gtk-serve` keeps a small layer table (4 entries) with the anchor, margins,
layer, buffer geometry and the pool descriptor. On a layer-surface commit it
composes the whole 1024x768 WIN1 frame: background clear, then layers from
background to overlay, honouring TOP/BOTTOM/LEFT/RIGHT anchors and margins,
clipping to the output, swizzling ARGB to the RX 8888 layout the viewer
expects, and finally rewriting the header. It prints
`layer compose px=0x...` with the top-left pixel.

A dedicated `layer-serve` build (`-DSERVE_ONLY -DSERVE_LAYER`) forks
`/sfs/wlclient layer`, a new minimal mode that creates a 256x64 red TOP|LEFT
layer surface with an shm buffer. Verified:

```
layer compose px=0xff0000ff
layer client ok
layer server ok
```

Limitations to address in the multi-client refactor:

- `gtk-serve` still accepts a single connection; the 2-client poll loop in
  `wayland_real.c` needs to be shared with the sink build.
- The compositor still uses the most recently created buffer's geometry for
  the commit; a real fix keys `(width, height, stride, format, offset)` by
  buffer id.
- No input path yet (host viewer to seat).

### GTK4: closure is too large to merge into the GTK3 image

`fakeroot pacman` against the Arch Linux ARM mirrors works for staging
(`/tmp/alarm/pacman.conf`, aarch64 core/extra/alarm/aur), but the closure for
`gtk4 libadwaita gtk-layer-shell` is **213 packages / 198.9 MiB compressed**
before installation. Merging that into the 256 MiB GTK3 image would overwrite
the working GTK3/glib stack and cannot fit the guest's 256 MiB of RAM. The
next step is a **separate GTK4 image** built from an ALARM root (or a
minimal set of *new* libraries: gtk4, graphene, libadwaita, gtk4-layer-shell,
gdk-pixbuf loaders), not a merge. The layer-shell server work above does not
depend on this.

## GTK4 image staged from Arch Linux ARM

`fakeroot pacman` against `mirror.archlinuxarm.org` works as a non-root
package stager (`/tmp/alarm/pacman.conf`, `Architecture = aarch64`,
`RootDir = ~/phosh-state/gtk4-root`, `--noscriptlet`). The full closure of
`gtk4 libadwaita gtk4-layer-shell` is 213 packages / 198.9 MiB compressed;
the installed root is ~1.2 GiB.

`tools/gtk4_closure.py` walks DT_NEEDED from the three libraries, and
`tools/merge_gtk4.py` merges only the files whose bytes differ from the GTK3
image (61 of 116 libraries were already byte-identical, so only 47 MiB were
added). The result is `~/phosh-state/disk_gtk4.img` (256 MiB image, 101 MiB
free) with `gtk4hello` (GTK4 window, quits after 2 s) and `gtk4-serve`
(the layer-capable compositor built with `SERVE_ONLY + SERVE_GTK4` which
forks `/sfs/gtk4hello` and sets `GSK_RENDERER=cairo`, `GTK_A11Y=none`).

One soname exceeded the SFS 27-byte name limit
(`libcairo-script-interpreter.so.2`, only needed by libgtk-4), so it was
renamed to `libcairo-script-interp.so.2` with `patchelf --set-soname` plus
`--replace-needed` in libgtk-4.

First boot of `linuxrun /sfs/gtk4-serve`: the compositor maps the sink and
the GTK4 client dies immediately with `free(): invalid pointer` (SIGABRT),
before any window appears. All GTK4 core dependencies (glib, gobject, gio,
pango, cairo, gdk-pixbuf, harfbuzz, wayland, xkbcommon) are byte-identical
to the GTK3 image, so this is not an ABI mix — it is the open heap
corruption below. GTK4 is the first workload that hits it on startup.

### Heap corruption: the writer is asynchronous

New facts from the static-glibc chunktest (block at `0x40801170`, 200 KiB):

- A **second immediate `memset` does not help**: the block still corrupts
  (round 606, 597 with `-smp 1`). So the damage is not a lost store from the
  first memset; something writes **between the memsets/verify**.
- The bad run starts at a 16-byte-aligned offset and extends to the **end of
  the block** (e.g. `11232..204800`); the header before the block is intact.
- The content is the constant 16-byte record
  `{u32 0x218, u32 0, u64 0}`. That is the exact layout of a
  `Vec<(u32, u64)>` element, i.e. an epoll result `(events, data)` with
  `events = EPOLLWRBAND|EPOLLERR|EPOLLHUP`, `data = 0`.
- `brk` is quiet: query, one small growth to `0x40800AE0`, one growth to the
  2 MiB cap `0x40A00000`; **no shrink**, so trim/regrow is not involved.
- `-smp 1` reproduces, so the writer is not another CPU.
- The kernel heap is `0x40526000..0x42526000` (32 MiB, `HEAP_PAGES=8192`);
  the corrupt frames are around `0x42BD6000`, above it, so the record is
  not the kernel heap's epoll `Vec` storage. The writer is still open.

Next probe: a QEMU gdb hardware watchpoint on the user VA's last 8 bytes and
on the kernel linear alias (`KERNEL_VIRT_BASE + PA + 0x1FF8`) to catch the
writer's PC, or a kernel PTE read-only trap to tell a user write from a
kernel linear-map write. QEMU HMP `pmemsave` rejected the filename argument
("invalid char 't' in expression"), so use the gdbstub instead.

### Heap corruption: no CPU writer, DMA is the remaining suspect

Watchpoint run on the base image with `-smp 1` (`watchtest`: first block at
`0x40801170`, 200 KiB, then free/alloc rounds):

- Read the block's tail PA (`0x42aa8000`) through a debug syscall and set a
  QEMU gdb hardware watchpoint on the kernel linear alias
  `0xffff000042aa7168`. It **never fired** during the whole 70 s run, while
  the block still corrupted (round 235, `pa_now == pa_start`).
- A page-table walk of the task found **576 mapped pages and zero duplicate
  VAs**; `refcount` of the corrupt frame is **1**; so no same-task alias and
  no shared frame.
- PTE is stable: the PA of the tail page before and after the corruption is
  the same (`0x42aa8000`).

A CPU write through the kernel linear map, another mapping of the frame, and
a cross-task share are therefore all ruled out for this reproduction. GDB
watchpoints only see CPU accesses, so **device DMA** is now the main
suspect: a descriptor/buffer that still points at a frame later handed to
the user heap. Candidates: the virtio-blk request/used ring (the block PA
must be compared with `ring_pa`/`request_pa` from `virtio_blk::init`) and
the net driver's RX/TX buffers (`userspace/src/bin/net_driver.rs`).

The constant record is `{u32 0x218, u32 0, u64 0}`; a virtio descriptor is
16 bytes and a used-ring element/request header is 8-16 bytes, so a stale
DMA descriptor/buffer is a good structural fit.

Next: print the virtio-blk ring/request PAs and the net-driver buffer PAs at
boot, compare with the user block PA; or run QEMU with a DMA trace. Also note
the `-smp 1` boot emits ~145k `MUTEX: unregister with no current task` lines
(a boot diagnostic loop before a current task exists); production boots with
`-smp 2` and does not show it, but it makes single-CPU debugging noisy.

### Heap corruption: watchpoint validated, stale-TLB ruled out

The QEMU gdb hardware watchpoint works: on `-smp 1` it fired on the user
VA of the block tail (`0x40833168`) with the memset's pattern
(`0x0101010101010101`). The earlier silent run was therefore meaningful:
a watchpoint on the **kernel linear alias** of the tail frame never fired
while the block corrupted, so there is no CPU write through the kernel map.

Other results:

- Anonymous `mmap` frames are properly zeroed (8 rounds of 512 KiB read all
  zero after `munmap`/`mmap`), so the `{0x218, 0}` content is written, not
  left over from an unzeroed frame.
- Refusing `brk` growth (forcing glibc to use mmap for the whole heap) does
  **not** stop the corruption; the block then lives in the mmap region and
  still corrupts.
- In that mode the block's VA/PA changes across rounds (glibc `mmap`/`munmap`
  per allocation), so comparing a boot-time PA with the corrupt round's PA is
  invalid. The 16-byte record follows the *current* block.
- Calling `mprotect(block, PROT_READ|PROT_WRITE)` after detection (a TLB
  flush) does not change the mismatches (`1346` before and after), so a
  simple stale TLB for the block VA is ruled out.
- The page table has no duplicate user VAs, the frame refcount is 1, and a
  cross-task scan of the corrupt tail PA finds only the current task.

Also fixed the `-smp 1` boot flood: `unregister_mutex` no longer prints when
there is no current task (that print re-entered the console lock and emitted
~145k lines before the shell).

Next: log the block VA and PA **per round** and watch a round that is stable
shortly before the failure, or write-protect the block page in the PTE and
trap the first writer (a kernel linear-map write will not fault, a user or
user-VA write will).

## 2026-11-11 — ROOT CAUSE FOUND: FP/SIMD state was not context-switched

The heap corruption that blocked GTK4/Phosh is fixed. The writer was **not**
device DMA, a stale TLB, a second mapping, or a kernel linear-map store: the
kernel never saved or restored the FP/SIMD register file (`q0..q31`, `FPCR`,
`FPSR`) across tasks. Every task shared one physical vector register bank.

### How it produced the `{0x218, 0}` suffix

glibc's `__memset_generic` (and `__memset_kunpeng`) keeps the fill pattern in
`v0` and writes it with `stp q0, q0` / `str q`. When a timer tick or a
migration on the other CPU switched tasks in the middle of the memset, the
incoming task's (or the kernel's) vector state replaced `v0`. The remaining
stores then wrote that vector state — a constant 16-byte record — from the
switch point `X` to the end of the buffer. That is exactly the observed
shape: a 16-byte-aligned suffix of repeated `{u64 0x218, u64 0}`, ending at
the malloc block boundary, with the prefix still holding the pattern.

Proof (`simdtest.c`): set `v0` to `0x55…`, `nanosleep` (forces a context
switch), read `v0` back. Before the fix every round returned `0x218`; after
the fix every round returns `0x55…`.

Why the earlier evidence pointed elsewhere:

- a QEMU gdb watchpoint on the kernel linear alias of the frame never fired
  (the clobbering store is a **user** store at the block's own VA);
- write-protecting the block's PTE did not fault (the store targeted the
  same, writable VA — the *value*, not the address, was wrong);
- the frame's PA/PTE never changed and a full `tlbi vmalle1is` did not help;
- a TCG plugin proved the corrupting store was never a `0x218` store on
  vCPU0 — the `v0` clobber happened in the *other* task's context, and the
  plugin's vCPU1 coverage was unproven (`c1=0` for every run);
- `-smp 1` with the timer pushed 20 s out (`cntp_tval_el0`) never corrupted,
  and `-smp 1` in general was clean: the switch has to land *inside* the
  ~200 KiB memset.

### Fix

- `smp::FpState` (`q[32]`, `fpcr`, `fpsr`, 16-byte aligned) and
  `PerCpu::fp` as the first `#[repr(C)]` field (offset 0, `FP_OFFSET`).
- `silver_fp_save` / `silver_fp_restore` in `smp.rs` (`stp q`/`ldp q`).
- Every EL0 entry saves the live file in `PerCpu::fp`; every EL0 exit
  restores it immediately before `eret`. Only exceptions whose SPSR says
  EL0t do this, so kernel-mode interrupts do not overwrite the task file.
  Sites: SVC entry/epilogue, IRQ entry/epilogue (conditional), user-fault
  entry/recovery, `silver_user_bootstrap`, `silver_fork_bootstrap`.
- `Task.fp` holds the parked file. `schedule()` swaps `PerCpu::fp` with the
  outgoing/incoming task just before `silver_context_switch`; `start()`
  installs the first task's file.
- `fork`/`clone` copy the **live** `PerCpu::fp` (the caller is inside its
  own syscall); `exec` resets the file to zero.

### Secondary fixes needed to reach GTK4

- `linux_mmap`: anonymous mappings are now **lazily backed** (`let lazy =
  true`). Eager backing charged an 8 MiB `pthread_create` stack as 2048
  frames against the per-task budget even though only a few pages are used.
  This is what made `g_thread_pool_new` return NULL and `GTask` assert in
  `g_task_thread_pool_init` (GLib 2.90 creates a `pool-spawner` thread for
  non-exclusive pools; if `pthread_create` fails it returns NULL).
- `mem_policy`: per-task limit is now **3/4** of usable frames instead of
  1/2 (`TASK_LIMIT_NUMERATOR/DENOMINATOR`). A GTK4 process maps ~120 MiB of
  libraries on a 256 MiB machine (`libicudata` alone is 31.6 MiB). The
  kernel reserve checked by `user_alloc_ok` is unchanged.

### Results

- `simdtest`: `before == after` on every round.
- Corruption test (`transient1`, 200 KiB memset × 6000 rounds, `-smp 2`):
  `transient mode=1 ok`, no corruption.
- `tools/linux-test.sh` + 32 guest tests (hello, chain, rw, libc, pie,
  mmapfile, cowfork, threads, unixsock, namedsock, signal, sighandler,
  asyncsig, pipe, ipc, scope, dyn, dynsig, pthread, pthread-exit,
  threadfork, memfd, futexping, eventloop, scratch, sockets, groupsig,
  exitgroup, dbus, wlreal, glibtest, giotest): **32/32 OK**.
- GTK4 image (`linuxrun /sfs/gtk4-serve`, which forks `/sfs/gtk4hello`):
  no more `free(): invalid pointer`, no `GTask` assertion, no `TASKLIMIT`;
  the client exits 0 and the compositor prints `gtk4 client ok`.
  `gtk4hello` standalone now only reports `Failed to open display` when no
  compositor is running (expected).

### Still open

- GTK4 client does not yet commit an shm frame (`gtk-serve: shm frame
  committed` is not printed): the mini compositor's protocol surface still
  needs what GTK4 waits for (surface configure / xdg-shell details). No
  crash anymore, so this is protocol work, not stability.
- Phosh itself (GTK4 + libadwaita + gtk4-layer-shell) is not started yet.
- Multi-client compositing and host input injection remain.
- `gdk-pixbuf` PNG loader recognition (`Couldn't recognize image file
  format`) remains.
- Host NVMe medium errors are unrelated but remain a hardware risk.

## 2026-11-11 — GTK4 window renders (xdg-shell role classification)

With the FP/SIMD fix and the memory-policy changes in place, `gtk4-serve`
still stopped after `gtk-serve: screen mapped`: the GTK4 client connected,
created its toplevel and committed a bufferless surface, but never received
`xdg_surface.configure`, so it never attached a buffer.

`WAYLAND_DEBUG=1` on the client showed the request order: GTK4 creates an
auxiliary `wl_surface` (fractional-scale/viewporter target) **before** the
toplevel. The compositor guessed roles from surface creation order — the
first surface became `surface_id`, the second `popup_surface_id` — so the
toplevel's `xdg_wm_base.get_xdg_surface` was classified as a popup and its
`get_toplevel` never matched `xdg_surface_id`; no configure was sent.

### Fix (`tests/linux/wayland_real.c`)

- `get_xdg_surface` records the xdg_surface/surface pair but **does not**
  guess the role.
- The role is decided by the next request on that xdg_surface:
  `get_toplevel` (opcode 1) makes it the toplevel and sends the configure;
  `get_popup` (opcode 2) moves it to the popup slot and restores the
  toplevel surface for enter/focus.
- `toplevel_surface_id` is part of the per-client `ClientState` and the
  `CLIENT_SCALARS` save/load list. Missing that made the second client
  inherit the first client's toplevel surface id; the source client then
  received `wl_keyboard.enter` naming its `wl_data_source` object
  (`invalid object (25), type (wl_data_source), message enter(uoa)`).

### Verification

- Client protocol log: `xdg_surface.configure` → `ack_configure` →
  `wl_shm.create_pool`/`create_buffer` (1080x826, stride 4320) →
  `attach` → `commit`; the compositor prints
  `gtk-serve: shm frame committed`.
- Host viewer: `snapshot: 1080x826 frame=2 nonwhite-samples=12007`; the PNG
  shows the real GTK4 window (title "Werewolf GTK4", buttons, label
  "GTK4 on WerewolfOS").
- `linux-wlreal`: `clipboard source ok`, `layer-shell ok`,
  `libwayland 2 clients ok`.
- `layer-serve`: `layer compose px=0xff0000ff`, `layer client ok`,
  `layer server ok`.

Rebuild the compositor into the GTK4 image with:

```
aarch64-linux-gnu-gcc -static -nostdlib -ffreestanding -O2 \
  -DSERVE_ONLY -DSERVE_GTK4 -Wl,-T,tests/linux/wayland_real.ld \
  -Wl,-z,max-page-size=4096 -o /tmp/opencode/gtk4-serve tests/linux/wayland_real.c
python3 scripts/sfs_inject.py ~/phosh-state/disk_gtk4.img /tmp/opencode/gtk4-serve gtk4-serve
```

### Next

Phosh: it needs GTK4 + libadwaita + gtk4-layer-shell in a dedicated image
(the staged closure was 213 packages / 198.9 MiB compressed; the current
GTK4 image has ~101 MiB free). The compositor now supports the layer-shell
handshake and xdg toplevels; the remaining work is the image, then the
Phosh process tree.

## 2026-11-11 — Phosh: from "lacks globals" to the gdk-pixbuf icon loader

State after the FP/SIMD fix (`d26d01e`) and the xdg role fix (`0896c41`).

### Phosh image

`~/phosh-state/disk_phosh.img` (256 MiB) is a copy of `disk_gtk3.img` plus:

- `/sfs/phosh` (from the Arch ARM `phosh` 0.57.0 package; interpreter rewritten
  to `/sfs/ld-linux-aarch64.so.1`, rpath `/sfs`),
- the DT_NEEDED closure of `phosh`: 41 packages, ~42 MiB of new libraries
  (`libhandy`, `libgnome-desktop-3`, `libgnome-bluetooth-3`, `libgcr`,
  `libappstream`, `libpulse`, `libnm`, `libmm-glib`, `libsecret`,
  `libupower-glib`, `libpolkit`, `libfeedback`, `libcallaudio`, …),
- phosh's `share/` data (schemas, icons, applications; locale and >27-byte
  names skipped — the SFS name limit is 27 bytes) and a merged
  `gschemas.compiled`,
- `glycin-image-rs` copied to `share/glycin-loaders/2+/` (see below),
- `/sfs/phosh-serve`, the compositor built from `wayland_real.c` with
  `-DSERVE_ONLY -DSERVE_PHOSH` (it execs `/sfs/phosh`).

Reproduce with `tools/build_phosh_image.py` after staging the packages into
`/tmp/alarm` (aarch64 pacman: `RootDir=~/phosh-state/gtk4-root`,
`DBPath=/tmp/alarm/db`, `mirror.archlinuxarm.org`; the GTK4 closure is
`~/phosh-state/gtk4-root`). `/tmp` does not survive a reboot, so the staging
root must be recreated.

### Compositor globals Phosh requires

`phosh-wayland.c` hard-requires `wl_output`, `zwlr_layer_shell_v1`,
`ext_idle_notifier_v1`, `xdg_wm_base`, `zxdg_output_manager_v1`,
`zwlr_output_power_manager_v1` and `zphoc_layer_shell_effects_v1`; the
monitor manager additionally calls `zwlr_output_manager_v1_add_listener`
unconditionally. `wayland_real.c` now advertises and parses all of them:

- `ext_idle_notifier_v1` (get_idle/get_input_idle_notification, replies
  `resumed`), `zwlr_output_power_manager_v1` (replies `mode(on)`),
  `zphoc_layer_shell_effects_v1` v3 (draggable/alpha/stacked requests are
  parsed and ignored), `zwlr_foreign_toplevel_manager_v1` (stop),
  `zwlr_gamma_control_manager_v1` (get_gamma_control replies `gamma_size(256)`,
  `set_gamma` closes the passed fd), `zwlr_output_manager_v1` (configuration
  requests are parsed, apply/test reply `succeeded`).
- New per-client state: `foreign_toplevel_manager_id`, `gamma_manager_id`,
  `gamma_control_id`, `output_manager_id`, `output_config_id`,
  `idle_notifier_id`, `idle_notification_id`, `output_power_manager_id`,
  `output_power_id`, `layer_effects_id`, `draggable_id`, `alpha_id`,
  `stacked_id`. Keep new fields in `CLIENT_SCALARS`, otherwise a second
  client inherits the first client's object ids (this already bit us once).

Debugging technique that found each NULL: run with `WAYLAND_DEBUG=1`, take the
faulting ELR from the kernel log, subtract the library base from the
`LINUX: fmap` lines, and inspect the GOT relocation of the interface pointer
(`readelf -r`); the interface name tells which proxy was NULL.

### Kernel: memfd shadow removed

A 64 MiB `ftruncate` (pulseaudio's mempool) used to allocate a full shadow
copy in the 32 MiB kernel heap and panic (`ALLOCATION ERROR`). `MemFdState`
now has an explicit `len`; memfds above `MEMFD_SHADOW_LIMIT` (1 MiB) are
frame-backed and `data` is dropped. Small memfds keep the heap shadow.
`read`/`write`/`pread`/`pwrite`/`truncate`/`seek`/`stat` use `len`. The
64 MiB memfd now fails with ENOMEM from the contiguous frame allocator
(non-fatal: pulse is not present anyway).

### Where Phosh stops now

Phosh starts, creates its shell, top panel, monitor and heads, then aborts in
GTK icon loading:

```
Gtk:ERROR: gtkiconhelper.c:495: ensure_surface_for_gicon: assertion failed
(error == NULL): Failed to load /org/gtk/libgtk/icons/32x32/status/image-missing.png:
Unrecognized image file format
```

`pixbuftest` fails the same way (`Couldn't recognize the image file format`),
while `glycintest` decodes the same PNG (`glycin ok 64x64`). gdk-pixbuf is
built with glycin (Arch 2.44.8: `libgdk_pixbuf` NEEDS `libglycin-2.so.0`), and
the glycin *loader binaries* were at `usr/lib/glycin-loaders/2+` but the
library searches `$XDG_DATA_DIRS/glycin-loaders/2+` (`/sfs/share/...`), which
only had `conf.d/`. The last change copies `glycin-image-rs[.real]` into
`share/glycin-loaders/2+/` and adds
`GLYCIN_DISABLE_SANDBOX=i-know-the-risks` to the Phosh environment (the
sandbox needs namespaces bwrap cannot create here; `glycintest` used the same
switch). **That run was interrupted before its result was captured** — the
next step is to re-run `linuxrun /sfs/phosh-serve` and check whether the icon
assertion is gone.

Non-fatal noise seen before the abort (useful when reading logs):

- `Failed to read /etc/glib-2.0/settings/*` — no native `/etc`; GSettings
  falls back to defaults.
- `ftruncate() failed: Cannot allocate memory`, `/tmp/pulse: Function not
  implemented`, `Gvc-WARNING: Failed to connect context` — no pulseaudio.
- `Missing phosh_private protocol extension` — keyboard grab/OSK features
  disabled; add a `phosh_private` stub later if needed.
- `phosh_osk_manager ... assertion 'self->proxy' failed` — no D-Bus/machine-id.
- `PolKit ... No session for pid` — no logind.
- `Gtk-WARNING ... pixbuf loaders or mime database could not be found` — the
  same gdk-pixbuf issue.

### Next steps

1. Re-run `linuxrun /sfs/phosh-serve`; if the icon assertion persists, verify
   the glycin loader search path (try `XDG_DATA_DIRS=/sfs/share:/sfs/usr/share`
   with the loaders also in `share/glycin-loaders/2+`), or provide a
   `loaders.cache` for gdk-pixbuf.
2. Once the shell renders, capture the WIN1 frame with
   `build/window-viewer/werewolf-window --snapshot`.
3. Then: multi-client compositing, host input injection, OSK, and the
   remaining Phosh session pieces (D-Bus, machine-id).

## 2026-11-11 — Phosh renders its lockscreen

The desktop is on screen. With the compositor serving Phosh's layer
surfaces, the host viewer captures a real Phosh lockscreen (1024x768):
big `00:01` clock, `Thursday, January 1`, and the unlock prompt. The
snapshot is `frame=38 nonwhite-samples=48004`.

Guest RAM is now **1 GiB** (`src/memory.rs` and `build_and_qemu.sh -m 1024M`);
the Phosh image is **2 GiB** (`~/phosh-state/disk_phosh.img`, 32768 inodes,
sparse 167 MiB on disk). `scripts/sfs_resize.py` and `scripts/sfs_inject.py`
now mmap the image instead of loading it into a bytearray.

### Why Phosh stopped before

1. **gdk-pixbuf had no MIME database.** It selects the loader with
   `g_content_type_guess`, so every PNG failed with `Unrecognized image file
   format` even though the glycin loader worked. `share/mime/mime.cache`
   (from `shared-mime-info`) is now in the image.
2. **glycin's zbus reactor starved.** The loader spawned and exited, but the
   parent never read the reply. `ASYNC_GLOBAL_EXECUTOR_THREADS=4` in the
   launcher environment makes it deterministic (same workaround as
   `glycintest`).
3. **`phosh_private` was missing.** `phosh_app_tracker_init` calls
   `phosh_private_get_startup_tracker` unconditionally and crashed on the
   NULL proxy (`wl_proxy_get_version(NULL)`). The compositor now advertises
   `phosh_private` v7 and parses `rotate_display`, `get_xdg_switcher`,
   `get_thumbnail`, `get_keyboard_event`, `get_startup_tracker` and
   `set_shell_state` (plus the tracker/keyboard-event/xdg-switcher destroy
   requests).
4. **Missing GSettings schemas.** Phosh aborts on
   `org.gnome.settings-daemon.plugins.power`. The merged
   `gschemas.compiled` now includes `gnome-settings-daemon`, `gnome-shell`,
   `phoc` and `gsettings-desktop-schemas`.
5. **Forking a 120 MiB process copied every library page.** glycin spawns its
   loader with `fork`; the eager per-page copy of file-backed `MAP_PRIVATE`
   mappings exhausted DRAM (`EAGAIN`). `fork_current` now shares those frames
   copy-on-write and write-protects both sides. `cow_fault` re-points the
   existing PTE (`remap_user_pages_at`, not `map_user_page_at`, which refuses
   an existing mapping) and the region index no longer shadows the page index.
6. **The kernel reserve was too large.** At 256 MiB a 5 MiB GDK shm pool was
   refused. `RESERVE_DIVISOR` is now 16; with 1 GiB guest RAM there is ample
   headroom. The stale `task_frame_limit` unit test (written for a 1/2 limit)
   was updated for the 3/4 policy.

### Compositor: per-object state, not one global id

Phosh uses the display in ways the demo never did:

- It binds `wl_shm` **twice** (GDK's registry and Phosh's own). A single
  `shm_id` made the second bind shadow the first, so `create_pool` on the GDK
  proxy was ignored. `is_shm()`/`shm_add()` now keep every shm proxy.
- It creates several `wl_shm_pool`s and buffers on older pools. `pools[]` and
  `buffers[]` (in `ClientState`, saved/loaded per client) record every pool fd
  and buffer geometry. `create_buffer` is accepted on any live pool.
- It creates six layer surfaces (lockscreen, background, top panel, panel
  background, home, background). `layer_surfaces[]` maps each `wl_surface` to
  its `zwlr_layer_surface_v1` and its own size/anchor/margin/layer, so each
  one receives its own configure.
- `wl_surface.attach` records the buffer per surface; `commit` blits and
  releases **that** buffer, not the most recently created one. Without this
  GDK allocated a fresh 5 MiB pool for every frame.

### Verified

- Host unit tests: **221 passed**.
- Guest: `linux-fork` (`parent saw child 42`), `linux-cowfork`
  (`cow fork ok`), `linux-memfd` (`shared memfd ok`), `linux-threadfork`
  (`thread wait status=0`), `linux-wlreal` (`clipboard source ok`,
  `layer-shell ok`, `libwayland 2 clients ok`), `layer-serve`
  (`layer compose px=0xff0000ff`, `layer client ok`, `layer server ok`).
- Phosh: `Phosh ready`, `gtk-serve: shm frame committed`, host snapshot of
  the lockscreen.

### Next

1. Input injection from the host (pointer/keyboard into the compositor) so the
   lockscreen can be swiped away and the shell becomes usable.
2. A real background/icon theme in the 2 GiB image (the current background is
   white because `/usr/share/backgrounds/gnome/blobs-d.svg` is absent and the
   hicolor theme is incomplete).
3. D-Bus session + machine-id for OSK/polkit/logind; `phosh_private` keyboard
   grabs.
4. Memory/process management in the iOS direction: a memory-pressure signal
   (jetsam-like), per-process priority/QoS, and killing/suspending background
   tasks before the kernel reserve is reached.

## 2026-11-11 — Host input reaches Phosh

The WIN1 header already carried an input ring (`0x100`, 64 slots of 8 bytes:
kind, value, key, x, y; head at `0x20`, tail at `0x24`). The viewer now also
emits `kInputRelease` on mouse release, and the compositor pumps the ring:

- `receive_message` waits with `ppoll` for at most 20 ms; a timeout lets
  `serve` call `poll_input` while the client is idle. A partial message is
  completed before returning (`incoming` is shared by all clients).
- `poll_input` forwards move/click/release/scroll to `wl_pointer` and keys to
  `wl_keyboard`, in surface-local logical coordinates (`x/2`, `y/2` for
  `wl_output.scale=2`).
- The pointer/keyboard `enter` is retried until the seat proxies exist: the
  first buffer can be committed before `get_pointer`/`get_keyboard`, and the
  old one-shot flag lost the enter forever. The first enter uses (100,50) as
  the demo client expects.
- The compositor clears the input ring when it maps `/dev/ivshmem`, so a
  previous run's swipe does not replay into a fresh boot.

Phosh's lockscreen is a layer surface, so `focus_surface` falls back to the
committing surface (not only an xdg toplevel). A host swipe now drags the
lockscreen up and reveals the home screen; a downward swipe from the top
opens the quick-settings panel (Cellular/Wi-Fi/Bluetooth tiles, brightness and
volume sliders, Landscape). Pointer focus is still "last committed surface",
so hit-testing by layer/position is the next refinement.

Tests stay green: `linux-wlreal` (clipboard, layer-shell, two clients) and
`layer-serve` (compose, client, server).

## 2026-11-11 — First iOS-style memory primitive: pressure levels

`mem_policy::pressure_level(free, total)` classifies the machine as `Normal`,
`Warning` (free at or below twice the kernel reserve) or `Critical` (free at or
below the reserve, i.e. user allocations are about to fail). The native ABI
exposes it as `SYS_MEMORY_PRESSURE` (91), returning 0/1/2.

This is the hook a future jetsam/notification path needs: a consumer can shed
caches at `Warning` before the allocator starts refusing at `Critical`. The
next steps are (a) a per-process priority/QoS so background loaders are the
first jetsam candidates, and (b) killing/suspending those tasks at `Critical`
instead of letting a foreground process fail an allocation.

Host tests: 222 passed. Guest boot and `linux-hello` (exit 42) still work.

## 2026-11-11 — One-command Phosh launch

`linuxstart <app>` resolves `<app>-serve` first, then `<app>`, under `/sfs`
and `/sfs/apps`, so `linuxstart phosh` runs `/sfs/phosh-serve` without the user
remembering the compositor suffix. `phosh` is a shortcut for the same.

`./build_and_qemu.sh --phosh` boots `$PHOSH_IMAGE` (default
`~/phosh-state/disk_phosh.img`) directly instead of `disk.img`, so the tracked
64 MiB test image is never overwritten. At the prompt:

```text
linuxstart phosh
```

## 2026-11-11 — Pointer hit-testing by layer, position and input region

The compositor used to give pointer and keyboard focus to "the last surface
that committed", so a swipe could land on the wrong layer. Input now follows
what is actually on screen.

### Hit-testing

`layer_rect()` is the single anchor/margin calculation shared by
`layer_compose` and the hit test, so pixels and input agree. `layer_hit_test()`
returns the topmost surface under a physical output point:

- higher `zwlr_layer_surface_v1` layer wins (background < bottom < top <
  overlay);
- within one layer the surface created last wins (`seq`, not commit order —
  otherwise the 60 Hz redraw reordered the stack every frame).

`pointer_route()` sends `leave`/`enter`/`motion` to the target, with
surface-local logical coordinates (`(px - rect.x) / 2` at scale 2).
`keyboard_route()` gives the keyboard to the topmost surface that asked for
it (`set_keyboard_interactivity`, exclusive wins regardless of the pointer),
falling back to the focused toplevel. Both are re-evaluated on every commit,
which also retries the first enter until the seat proxies exist.

### wl_surface input regions

`wl_compositor.create_region`, `wl_region.add/subtract/destroy` and
`wl_surface.set_input_region` are implemented. A surface with an empty region
takes no pointer input; a non-empty region is hit-tested against its bounding
box. Phosh marks panel shadows and backgrounds this way, and without this they
swallowed every click meant for the panel.

### Compositor bugs found while testing

- **Buffer table overflow.** `wl_buffer.destroy` was not handled and
  `buffer_add` recycled slot 0 when full, silently handing a live buffer
  another buffer's fd/offset/size. Phosh creates one buffer per frame, so the
  table filled within seconds and layers drew each other's content (the
  duplicated quick-settings panel). The table is now 512 entries,
  `buffer_add` refuses instead of recycling, `wl_buffer.destroy` frees the
  slot, and a commit whose buffer is unknown is skipped rather than blitted
  from stale globals.
- **Composed frames were invisible.** `layer_compose` wrote `frameSeq = 0`
  after drawing, so the host viewer kept showing the last single-surface blit
  instead of the composition. It now writes a monotonically increasing
  sequence, which is also why the duplicated panel and the quick-settings
  layout are only visible now.
- **Wrong logical output size.** `zxdg_output_v1.logical_size` reported
  1024x768 while `wl_output` mode is 1024x768 at `scale=2`, i.e. a 512x384
  desktop. Phosh sized its panels against a screen twice as large as the one
  it was drawing.
- `attach(NULL)` now removes the layer entry, so a hidden surface stops being
  drawn and stops taking input.

### New regression test

`layer-hit` (a `SERVE_HIT` mode of `wayland_real.c` running `wlclient hit`)
creates three layer surfaces:

| surface | layer    | geometry                  | input region |
|---------|----------|---------------------------|--------------|
| a       | top      | 400x300 at the top-left   | empty        |
| c       | overlay  | 200x200 at bottom-right   | full         |
| b       | bottom   | full screen               | full         |

The compositor's first enter is at (200,100) physical, inside `a`, so it must
skip `a` (empty region) and land on `b`. Host moves to (900,700) and (500,400)
must produce `c` and `b`, so the expected enter sequence is `bcb`:

```text
hit sequence=bcb
hit test ok
hit server ok
```

### Verified

- Host unit tests: **222 passed**.
- Guest: `linux-wlreal` (clipboard, layer-shell, two clients), `layer-serve`
  (compose/client/server), `layer-hit` (`hit test ok`).
- Phosh still reaches `layer compose` and paints; the quick-settings panel is
  no longer duplicated.

### Still open on the Phosh side

- Phosh starts unlocked and with the quick settings expanded; its top panel is
  resized to 768 logical (the physical output height) instead of 384, so the
  panel is laid out against the wrong screen height. It does not react to
  swipes on the panel yet. Re-sending a `configure` when the client changes
  `set_size`/`set_anchor` is the next protocol gap, but the first attempt made
  the home clock clip, so it needs a careful look.
- The home screen is black: no wallpaper and no icon theme (next task).
- `zphoc_layer_shell_effects_v1.stack_below()` is still ignored, so Phosh's
  "background" surfaces are ordered by creation instead of by the requested
  stacking.

## 2026-11-11 — Wallpaper, symbolic icons and persistent buffers

### The buffer was being dropped after every commit

Wayland says a `wl_surface.commit` without `attach` keeps the current buffer;
only `attach(NULL)` clears it. The compositor cleared `attached_buffers[]` after
every commit, so GTK's repeated "present without attach" looked like
`attach(NULL)`: the layer entry was forgotten, the surface vanished, and the
screen flickered black or showed only part of a layer. Keeping the attachment
fixes the flicker and the half-drawn wallpaper.

### Theme assets

`tools/gen_theme_assets.py` draws everything from scratch (no third-party
artwork):

- a 1024x768 night-sky wallpaper (`share/backgrounds/werewolf.png`), and
- 59 monochrome symbolic icons for the names Phosh asks for (cellular/wifi
  signal levels, bluetooth, brightness, volume, battery, arrows, padlock,
  gear, refresh, `image-missing`).

The SFS directory entry holds at most 27 bytes of name, and names such as
`network-wireless-signal-excellent-symbolic.png` are longer. The icons are
therefore packed into `theme.pack` (magic `WPK1`: u16 name, u32 length, bytes)
and `phosh-serve` unpacks it into `/tmp/xdg/share` before it execs Phosh, with
`XDG_DATA_DIRS=/tmp/xdg/share:/sfs/share:/sfs/usr/share`.

### GSettings without dconf

There is no D-Bus/dconf in the guest, so GLib falls back to its keyfile
backend. `phosh-serve` now writes `/tmp/.config/glib-2.0/settings/keyfile`
with `org.gnome.desktop.background.picture-uri` (the wallpaper),
`picture-options=zoom` and `org.gnome.desktop.interface.icon-theme=hicolor`
before starting Phosh. Phosh then paints the wallpaper and finds the icons.

The builder (`tools/build_phosh_image.py`) now generates and injects the
wallpaper and `theme.pack` automatically, so rebuilding the image keeps them.

### Verified

- 222 host unit tests; `linux-wlreal`, `layer-serve` and `layer-hit` still
  green.
- Phosh paints the wallpaper (moon, stars, pines), the clock, and the quick
  settings with a speaker icon, a chevron and the Bluetooth glyph. Log shows
  `theme: unpacked 59`.

### Known gaps

- Phosh still starts with quick settings expanded; its layer surfaces are
  sized against a taller screen than 384 logical, so the home surface covers
  the top of the wallpaper. The next protocol gap is sending a fresh
  `configure` after `set_size`/`set_anchor` (and honouring `stack_below`).
- The Cellular and Wi-Fi tiles fall back to `image-missing`; the exact icon
  names Phosh requests for them are not covered yet.

## 2026-11-11 — Resize configures, real timestamps, layer stacking (WIP)

### Fresh `configure` when the size changes

The compositor sent exactly one configure per layer surface. Phosh calls
`set_size(0, 384)` when quick settings opens, and without a new configure GTK
kept rendering a buffer for the old 32-logical height (504 physical rows).
`LayerSurface.pending` is set by `set_size` and a configure goes out on the
next commit. Margins/anchors deliberately do not trigger one: Phosh animates
margins and re-configuring on every frame caused a slow resize feedback loop.

### Monotonic event and frame timestamps

`frame.done`, pointer motion/button/axis and keyboard key events all carried
`time = 0`. GTK drives animations and gesture recognition from those values,
so every animation crawled and every swipe looked instantaneous. `now_ms()`
uses `clock_gettime(CLOCK_MONOTONIC)`. With real timestamps the Phosh startup
animation settles in a second and the home screen (wallpaper + clock) is
stable.

### Frame callbacks on commit

`wl_surface.frame` callbacks were completed immediately, so GTK rendered in a
tight loop and the guest spent its time recompositing. They are now stored per
surface and completed on that surface's next commit, with the presentation
timestamp.

### zphoc layer stacking

`zphoc_layer_shell_effects_v1.get_stacked_layer_surface` and
`stack_below`/`stack_above` are implemented. Phosh puts the panel and
lockscreen *backgrounds* below their content that way; without it those
full-screen backgrounds were on top (creation order) and took every pointer
event meant for the panel. Composition and hit-testing now share
`layer_entry_above()` (layer, explicit relation, then creation order).

### State

Builds clean; the wallpaper/timestamp run was verified in the guest (stable
home screen). The stacking change and deferred frame callbacks still need the
`linux-wlreal` / `layer-serve` / `layer-hit` regression run. One open issue:
a downward drag on the top panel does not open quick settings yet — the
instrumented build showed the initial pointer enter but the drag never reached
the panel, so hit-testing on that surface is still being checked.

## 2026-11-11 — Input actually works: wl_region opcodes, frames, implicit grab

Three bugs together kept every gesture from reaching Phosh:

1. **`wl_region` opcodes were shifted.** The interface is `destroy=0`, `add=1`,
   `subtract=2`; the compositor treated `add` as `subtract`, so every region
   stayed empty and `set_input_region` marked interactive surfaces (the top
   panel!) as input-transparent. With the correct opcodes the panel is hit
   (`HIT 512 4 -> 62`) and the lockscreen swipe-up unlocks to the home screen.
2. **No `wl_pointer.frame`.** The seat is advertised at version 5, where
   pointer events are grouped into frames; GTK buffered gestures waiting for
   the frame event.
3. **No implicit grab.** While a button is held, events now stay on the surface
   that received the press, even after the pointer leaves its rectangle.

`wlclient` gained no-op `.frame` listeners (libwayland aborts on a received
event whose listener is NULL) and the `layer-hit` window grew to ~20 s for the
slow guest.

Verified: `linux-wlreal`, `layer-serve`, `layer-hit` (`hit sequence=bcb`),
222 host tests. Phosh: swipe up unlocks to the wallpaper home screen.

Still open: pulling down the top panel does not open quick settings with a
pointer drag, at any start position. Phosh is a phone shell and its QS reveal
looks touch-only, so the next step is a `wl_touch` device (mouse drag -> touch
down/motion/up/frame).

## 2026-11-11 — A session bus for Phosh

`SO_PEERCRED` was already implemented, so the missing pieces for D-Bus were:

1. **No `/etc` in the root VFS.** `vfs::resolve` only normalizes, so the Linux
   read allowlist rejected `/etc/passwd` with EACCES, and `getpwuid(0)` then
   failed with ENOENT. `seed()` now creates `/etc` and seeds `passwd`, `group`,
   `nsswitch.conf` and `hostname`; the Linux read allowlist also accepts `/etc`.
2. **No `dbus-daemon` in the image.** It ships as `/sfs/dbus-daemon` (patched
   interpreter, `libgcc_s` NEEDED removed) with a permissive
   `/sfs/share/dbus-1/session.conf` listening on `unix:path=/tmp/dbus-session`.
3. **The daemon could not find `libgcc_s.so.1`.** Its `libsystemd` has no
   RUNPATH, so the loader fell back to `/lib`; the child now gets
   `LD_LIBRARY_PATH=/sfs`.

`phosh-serve` forks the daemon before Phosh and exports
`DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/dbus-session`.

Phosh now connects: the errors changed from "Could not connect: Connection
refused" to `org.freedesktop.DBus.Error.ServiceUnknown` for services that
simply do not exist yet (logind, NetworkManager, the OSK service). polkit still
reports "No session for pid 9" because there is no logind.

`layer-hit` is now event driven (it stops after the three expected enters)
because the guest clock runs fast and a tick budget is not a reliable timeout.

Verified: 222 host tests, `linux-wlreal`, `layer-serve`, `layer-hit`; Phosh
still paints the wallpaper and unlocks with a swipe up.

## 2026-11-11 — Frame clock ticks without a commit

GTK requests `wl_surface.frame` and waits for `done` before running its tick
callbacks. Completing the callback only on the surface's next commit stalled
every animation that had nothing new to draw — the top panel never finished
unfolding, so quick settings stayed off-screen. Pending callbacks are now also
flushed on the 20 ms idle timeout, which acts as a ~50 Hz vblank.

Verified: 222 host tests, `linux-wlreal`, `layer-serve`, `layer-hit`.

## 2026-11-11 — 3D acceleration: what it needs

`./build_and_qemu.sh --gpu` now attaches a virtio-gpu: `virtio-gpu-gl-pci`
when the QEMU build has OpenGL, otherwise the 2D `virtio-gpu-pci` (the current
QEMU 11.1.3 has only the 2D device installed).

Real 3D is a separate track, because three layers are missing:

1. **Host QEMU**: `qemu-hw-display-virtio-gpu-pci-gl` and `virglrenderer` are
   not installed (installing needs root). Without them QEMU has no
   `virtio-gpu-gl-pci` and no `egl-headless`, so virgl/venus cannot run.
2. **Kernel**: there is no virtio-gpu driver and no DRM device
   (`/dev/dri/card0`). This is the largest piece: virtio PCI probe + queues,
   resource create/attach/flush, a DRM ioctl surface (dumb buffers, addfb,
   page flip) and a virtio 3D context for virgl.
3. **Guest userspace**: the image has `libdrm.so.2` but no `libEGL`,
   `libGLESv2`, `libgbm` or `virtio_gpu_dri.so`. Mesa's virgl driver and an
   EGL/GBM path in the compositor are needed, plus `GSK_RENDERER=gl` for GTK4
   apps.

Phosh 0.57 itself is GTK3/cairo, so it would not use the GPU; the win is for
GTK4 apps and for compositing via dmabuf.

### virtio-gpu discovery

`src/virtio_gpu.rs` scans PCI for a virtio-gpu (0x1050 modern / 0x0010
transitional), enables the memory window and prints the device and its BARs.
It is wired into boot after the SFS mount, so `--gpu` boots show what the
device exposes before the control queue and DRM work start.

`--gpu` now detects the device through `-device help` (QEMU exits 0 for
`-device NAME,help` even when the device is unknown, which made the old check
always pass). The base modules `qemu-hw-display-virtio-gpu{,-pci}` are still
needed for any virtio-gpu device to exist.

### virtio-gpu is visible

With the base and GL modules installed, QEMU exposes `virtio-gpu-gl-pci` and
`egl-headless`. A `--gpu` boot now reports the device from the kernel probe:

```text
VIRTIO-GPU: device 4.0 id 0x0000000000001050
VIRTIO-GPU: BAR4 0x000000000000000C
```

BAR4 is unassigned (`0x0C` = 64-bit memory BAR, address 0), so the driver must
program a BAR address the way virtio-blk does. `--gpu` prefers the GL device
with `-display egl-headless` and falls back to the 2D device.

### virtio-gpu: control queue works

`src/virtio_gpu.rs` now claims the device (relocating the 64-bit BAR4 into
0x1020_0000), negotiates VIRTIO_F_VERSION_1, brings up the control queue
(queue 0, 64 entries) and issues `GET_DISPLAY_INFO`. A `--gpu` boot prints:

```text
VIRTIO-GPU: device 4.0
VIRTIO-GPU: display 0 1280x800
```

Two protocol details that cost a round trip each: the success code is
`VIRTIO_GPU_RESP_OK_DISPLAY_INFO = 0x1101` (the response enum starts at
0x1100), and `virtio_gpu_display_one` is `{ rect x,y,w,h; u32 enabled; u32
flags; }` — 24 bytes with `enabled` *after* the rectangle.

Next: `RESOURCE_CREATE_2D` + `ATTACH_BACKING` + `SET_SCANOUT` +
`TRANSFER_TO_HOST_2D`/`RESOURCE_FLUSH` so the compositor can present through
the GPU instead of the ivshmem blit, then a DRM device and the virgl context.

### virtio-gpu: 2D scanout, and the desktop presented through the GPU

The driver now creates a 2D resource (`RESOURCE_CREATE_2D`, format
`R8G8B8A8_UNORM`), attaches a contiguous framebuffer with `ATTACH_BACKING`,
points scanout 0 at it with `SET_SCANOUT`, and presents with
`TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`. `init` draws colour bars so a host
`screendump` proves the whole path before any client exists:

```text
VIRTIO-GPU: device 4.0
VIRTIO-GPU: scanout 1024x768 resource 1
```

`/dev/gpufb` exposes that framebuffer to userspace: `mmap` maps the frames
Normal Non-Cacheable (the same DMA protocol as every other surface) and
`ioctl(fd, 0x5701 /* _IO('W',1) */)` transfers and flushes the frame. The
compositor maps it at startup and, after each composed frame, copies the
ivshmem pixels into it and flushes, so both sinks stay correct: the host
viewer keeps reading the WIN1 BAR, and QEMU's own display head (or a
screendump) shows the GPU output. `--gpu` now asks for `xres=1024,yres=768`
so the scanout is exactly the compositor's output size.

Verified with a screendump of the Phosh lockscreen through the GPU path:
wallpaper, moon, clock and date all arrive with the right colours (a
byte-order mistake would show as red/blue swapped).

### DRM/KMS card0

`/dev/dri/card0` is a small DRM device on top of virtio-gpu:

- `DRM_IOCTL_VERSION`, `GET_CAP` (`DUMB_BUFFER`, `TIMESTAMP_MONOTONIC`),
- `MODE_GETRESOURCES` / `GETCRTC` / `GETENCODER` / `GETCONNECTOR` (one CRTC,
  one connected virtual connector, one 1024x768 preferred mode, two-pass
  pointer/count protocol),
- `MODE_CREATE_DUMB` / `MAP_DUMB` / `DESTROY_DUMB`,
- `MODE_ADDFB` / `ADDFB2` / `RMFB` (XRGB8888 and ARGB8888),
- `MODE_SETCRTC` and `MODE_PAGE_FLIP` with a `DRM_EVENT_VBLANK` on `read`.

A dumb buffer is a contiguous frame run. `MAP_DUMB` returns its physical
address as the mmap offset and `mmap` maps it Normal Non-Cacheable. The first
flip creates a 2D resource for the buffer (format `B8G8R8A8_UNORM`, which is
what an `XRGB8888` u32 looks like in memory), then every flip transfers and
flushes it and points scanout 0 at it. The 32-byte vblank event carries the
flip's `user_data` and is reported by `poll`/`read`.

`tests/linux/drmtest.c` walks the whole API: it prints the counts and mode,
creates and maps a dumb buffer, adds an fb, fills four quadrants and flips.
The log ends with `drm test ok`, `event bytes 32`, `event type 1`, and a host
screendump shows red/green/blue/white in the expected corners.

### virgl: 3D context and a virtgpu render node

`src/virtio_gpu.rs` now negotiates `VIRTIO_GPU_F_VIRGL` when the device offers
it and speaks the 3D command set: `CTX_CREATE`, `CTX_ATTACH_RESOURCE`,
`RESOURCE_CREATE_3D`, `TRANSFER_TO/FROM_HOST_3D`, `SUBMIT_3D`, plus
`GET_CAPSET_INFO`/`GET_CAPSET`. `SUBMIT_3D` stages the command buffer in a
128 KiB area and chains three descriptors (header, commands, response).

`GET_CAPSET_INFO` takes a **capset index** into the device's capset list
(QEMU's list is `[VIRGL, VIRGL2]`), while `GET_CAPSET` takes a capset **id**;
passing an id to the info query returns an all-zero response and cost a round
trip. The whole response page must be invalidated after a command: only
invalidating the 24-byte header left `GET_DISPLAY_INFO`/`GET_CAPSET` payloads
stale in the cache.

`/dev/dri/renderD128` exposes the virtgpu DRM API: `GETPARAM` (3D features,
supported capset ids), `GET_CAPS` (the device's 1408-byte virgl2 capset),
`CONTEXT_INIT`, `RESOURCE_CREATE`, `RESOURCE_INFO`, `MAP`, `EXECBUFFER`
(forwarded to `SUBMIT_3D`, with an eventfd fence when
`VIRTGPU_EXECBUF_FENCE_FD_OUT` is set), `TRANSFER_TO/FROM_HOST`, `WAIT` and
`GEM_CLOSE`. Resources are contiguous frame runs mapped Non-Cacheable.

`tests/linux/rendertest.c` walks that API. On a `virtio-gpu-gl-pci` boot it
prints `3d features 1`, `capset ids 6`, `caps bytes 1408`, creates a 64x64
texture (res handle 2), maps it, submits a virgl `NOP` and receives fence
fd 4.

What is still missing for `GSK_RENDERER=gl`: shipping the aarch64 Mesa
(`libEGL`, `libGLESv2`, `libgbm`, `libgallium`, `libdril_dri.so` as
`virtio_gpu_dri.so`) into the image, and a Wayland buffer-sharing protocol
(`zwp_linux_dmabuf_v1` or `wl_drm`) so a compositor can import GL-rendered
buffers. An offscreen EGL/GLES test only needs the Mesa files.

### Lockscreen flicker: one frame per commit

A layer surface was published twice per `wl_surface.commit`: the commit
handler blitted the client's buffer straight into the sink (the raw lockscreen
surface, no background), and then `layer_compose()` drew the full stack
(wallpaper + lockscreen) and published that. The sink therefore alternated
between two different images every commit, which showed up as the lockscreen
flickering with the wallpaper visible underneath.

The direct blit is the single-window fast path and now runs only when the
surface is not a layer surface (`layer_slot == 0`); layer surfaces are
published exclusively by `layer_compose()`. Eight screendumps of the running
Phosh lockscreen, ~20 s apart, are now byte-identical.

### EL0 system registers: UCT, DZE and UCI

The kernel set `SCTLR_EL1.M` and `PAN` but left `UCT`, `DZE` and `UCI`
clear. glibc's `memset` reads `CTR_EL0` to decide whether it may zero a block
with `DC ZVA`; without `UCT` that `MRS` traps to EL1 (`EC = 0x18`, which the
fault path printed as a page fault with a nonsense "alignment fault" DFSC —
`ISS` means something different for EC 0x18). Every process that memset a
large buffer died. The boot and AP paths now set all three bits, so EL0 can
read `CTR_EL0`, use `DC ZVA` and run `__clear_cache`.

### Mesa: EGL/GLES on the render node

`tools/install_mesa.py` resolves the aarch64 Mesa closure from a staged Arch
root, renames `libgallium-26.2.6-arch1.1.so` to `libgallium-26.2.6.so` (the
SFS name limit is 27 bytes), patches the soname and every `DT_NEEDED`, and
writes the files plus `egl_vendor.json` into the image. `libdril_dri.so` is
shipped both as itself and as `virtio_gpu_dri.so`.

`tests/linux/egltest.c` runs the stack with no window system: it preloads
libgallium (the DRI stub resolves the driver entry points from it), creates a
GBM device on `/dev/dri/renderD128`, gets an EGL display from
`EGL_PLATFORM_GBM_KHR`, makes a GLES2 context current, clears an FBO and reads
the pixel back:

```text
egl: version 1.5
gl: renderer llvmpipe (LLVM 23.1.4, 128 bits)
gl: framebuffer complete
gl: readback 64 128 191 255
egl test ok
```

`EGL_PLATFORM_DEVICE_EXT` returns `EGL_BAD_PARAMETER` (Mesa's device platform
enumerates DRM devices through libdrm/sysfs, which the guest has no sysfs
for); GBM takes the fd directly and works. The surfaceless platform with
`EGL_DEFAULT_DISPLAY` is swrast by definition, which is why the first attempt
reported llvmpipe.

Mesa reaches the render node (`GET_CAPS`, then `GETPARAM` 1..7) but asks for
capset **6** (`VIRTGPU_DRM_CAPSET_DRM`, size 160) before creating the context.
QEMU only exposes that capset with `drm_native_context=on`, which requires
`blob=on`; with blobs the modern virtio capability block moves from BAR4 to
BAR2 (BAR4 becomes the 512 MiB shared-memory BAR), and the driver's hardcoded
BAR4 window breaks — the 2D scanout stops working. Until the driver parses the
PCI capabilities and serves the DRM capset, Mesa falls back to llvmpipe.

Also raised `MAX_FILE_MMAP_BYTES` to 256 MiB: the loader maps libLLVM
(171 MiB) as one segment, and the old 64 MiB cap made `dlopen` fail. DRM and
`/dev/gpufb` mappings are now Normal Cacheable (the DMA path cleans before
every transfer), because `DC ZVA` on Non-Cacheable memory is not
architecturally defined.

### virtio-gpu: PCI capability parsing

The driver no longer assumes the modern virtio register banks live in BAR4
at fixed offsets. It walks the PCI capability list, collects the COMMON
(cfg_type 1), NOTIFY (2) and DEVICE (4) capabilities with their BAR, offset,
length and notify multiplier, programs those BARs from the 0x1020_0000
window and derives the register addresses from the capability data.

This matters as soon as the device has a shared-memory BAR: with
`blob=on,hostmem=...,drm_native_context=on` QEMU moves the modern block from
BAR4 to BAR2 (16 KiB) and gives BAR4 to the 256 MiB shared window, so the old
hardcoded window both failed the relocation check and stopped the scanout.

The default (non-blob) layout is unchanged: the capability walk finds the same
BAR4 offsets and the 2D scanout still reports `scanout 1024x768 resource 1`.

### Why Mesa still uses llvmpipe

`DRM_IOCTL_VERSION` now reports major 0 / minor 1 with the name `virtio_gpu`.
Mesa's virgl winsys treats a non-zero DRM major as a protocol error
(`virgl_drm_get_version` returns `-EINVAL`), so the old `major = 1` made it
drop the driver silently.

That was necessary but not sufficient. Tracing every ioctl on the render node
shows what Mesa 26.2 actually does:

```text
VERSION (x2), GET_CAPS(capset 6, size 160), GETPARAM 1..7, GET_CAPS(6),
GETPARAM 1..7, GET_CAP(DRM_CAP_PRIME) -> give up, fall back to llvmpipe
```

`GET_CAPS(6)` with a 160-byte buffer is `vdrm_virtgpu_connect` in
`src/virtio/vdrm/vdrm_virtgpu.c`, i.e. Mesa's **DRM native context** layer,
not virgl (`virgl_drm_winsys_create` would query capsets 1/2 with 1408 bytes
and then issue `VIRTGPU_CONTEXT_INIT`). Arch's Mesa 26.2 ships only
`virtio_gpu_dri.so` and its DRI stub exports only
`__driDriverGetExtensions_virtio_gpu`; there is no `virgl_dri.so` and no
`__driDriverGetExtensions_virgl`, so the virgl GL driver is not reachable in
this build at all.

The native path needs the DRM capset, which QEMU only exposes with
`drm_native_context=on` + `blob=on`, and virglrenderer then fails on this host
(`failed to initialize drm renderer`, ENODEV) because a native context
requires a host GPU driver for the virtual device.

So hardware GL is blocked in userspace, not in the kernel: either ship a Mesa
that still has the virgl driver (25.x), or use Venus/zink (`venus=on`,
`libvulkan_virtio.so`), or accept llvmpipe. The render node, the 3D context
and the capset plumbing are all in place and exercised by `rendertest`.

### Mesa 25 (Debian) with virgl: installed, still llvmpipe

Key finding: Mesa 26.2 (Arch) no longer exposes a virgl GL driver. Its
`virtio_gpu_dri.so` is the DRM *native context* driver (vdrm): libgallium
26.2 contains `could not get caps`, `libdril_dri.so` exports only
`__driDriverGetExtensions_virtio_gpu`, and there is no `virgl_dri.so`. The
native path needs QEMU `drm_native_context=on` (+`blob=on`), which makes
virglrenderer fail here (`failed to initialize drm renderer`, ENODEV).

Mesa **25.0.7** from Debian is the opposite: libgallium 25.0.7 contains the
virgl winsys strings (`DRM_IOCTL_VIRTGPU_CONTEXT_INIT failed`,
`No virgl contexts available`) and no vdrm, so `virtio_gpu_dri.so` there is
the virgl driver. Installed into the image from
`https://deb.debian.org/debian/pool/main/m/mesa/`:

- `mesa-libgallium_25.0.7-2+deb13u1_arm64.deb` (35 MiB, renamed to
  `libgallium-25.0.7.so`, soname + DT_NEEDED patched),
- `libegl-mesa0`, `libgbm1`, `libgl1-mesa-dri` (the `libdril_dri.so` stub and
  `dri/virtio_gpu_dri.so`),
- `libllvm19` (122 MiB `libLLVM.so.19.1`; it is a DT_NEEDED of libgallium),
- `libz3-4`, `libedit2`, `libbsd0`, `libmd0`, plus Arch's
  `libwayland-server.so.0` and `libtinfo.so.6` (copy of `libncursesw.so.6`).

`tools/install_mesa.py` now takes `--force` (overwrite image files), detects
`usr/lib/aarch64-linux-gnu`, seeds `mesa-libgallium`'s versioned soname and
shortens `libgallium-<ver>-<pkg>.so` to `libgallium-<ver>.so`.

`egltest` now loads Debian Mesa 25.0.7 (EGL 1.5, `OpenGL ES 3.2 Mesa
25.0.7-2+deb13u1`, LLVM 19.1.7) and renders/reads back correctly, but the
renderer is still **llvmpipe**.

Next step: re-enable the render-node ioctl trace (the `IOCTL <request> -> <rc>`
line in `linux_abi`) and run `egltest` once. With Mesa 25 the expected
sequence is `GETPARAM 1..7` then `VIRTGPU_CONTEXT_INIT` (0xC010644B) and
`RESOURCE_CREATE`; if it stops earlier, the failing request is visible
directly. Nothing else in the image needs to change.

### VIRTGPU_GETPARAM takes a pointer

`drm_virtgpu_getparam { u64 param; u64 value; }` — `value` is a **user
pointer** to the result, not the result. Mesa passes `&value` and reads its
own local afterwards. The driver wrote the answer into the struct field
instead, so Mesa always saw `3D features = 0` and dropped the render node
silently (no message at all).

With the pointer written correctly, Mesa 25.0.7 goes all the way through:

```text
GETPARAM 1..7 (all succeed)
drmGetVersion (major 0)
VIRTGPU_CONTEXT_INIT -> 0        (virgl context created)
GET_CAPS(2) -> 1408-byte virgl2 capset
```

and then fails at the EGL device check:

```text
MESA-LOADER: failed to retrieve device information
libEGL debug: ... DRI2: failed to get compatible render device
```

`loader_is_device_render_capable` calls `drmGetDevice2`, and libdrm answers it
by reading `/sys/dev/char/<maj>:<min>/device` (vendor/device/uevent, plus a
`subsystem` symlink). This guest has no sysfs, so the check fails and Mesa
falls back.

`tests/linux/drm_shim.c` is an experimental userspace answer: it provides
`drmGetDevice2`, `drmGetDevices2`, `drmGetDeviceFromDevId`, the free/equal
helpers and the node-name helpers, describing the device as a Linux guest
would see virtio-gpu (1af4:1050, primary + render nodes). With it Mesa creates
the virgl context and fetches the capset, then dies with a translation fault
inside libgallium's screen setup (`str` into an unmapped page just past a
lazy mapping) — the shim is not consistent enough yet.

Two ways forward:
1. Give the kernel a tiny synthetic sysfs (`/sys/dev/char/<maj>:<min>/device`
   with vendor/device/uevent and a `subsystem` symlink, plus `readlink` and
   `faccessat` for those paths). Then Mesa works unmodified and the shim goes
   away.
2. Debug the shim crash: the faulting write is at `0x40A04004`, just past a
   2-page lazy region, with the caller inside libgallium's `virgl_create_screen`
   vtable fill.

`egltest` now defaults to `MESA_LOADER_DRIVER_OVERRIDE=swrast` so the test is
deterministic (Mesa 25.0.7 + llvmpipe, `egl test ok`); build it with
`-DEGLTEST_VIRGL` to exercise the experimental virtio_gpu path with the shim.

`rendertest` used the old (wrong) GETPARAM semantics and started failing once
the kernel wrote through the pointer; it now passes `&value` like Mesa and
prints `3d features 1`, `capset ids 6`, `caps bytes 1408`, `render test ok`
on a `virtio-gpu-gl-pci` boot. `drmtest` and `layer-serve` stay green.

### Synthetic sysfs: libdrm accepts the render node

Mesa's loader asks libdrm to describe the device behind the fd, and libdrm
(on Linux) walks `/sys/dev/char/<maj>:<min>` and then **enumerates
`/dev/dri`** with `opendir`. Neither exists in this guest, so
`drmGetDevice2` failed with EACCES and Mesa reported "DRI2: failed to get
compatible render device".

`src/sysfs.rs` now answers exactly that:

- `/sys/dev/char/226:{0,128}/device/{vendor,device,subsystem_vendor,
  subsystem_device}` = 1af4 / 1050 / 1af4 / 1100,
- `device/uevent` with `DRIVER=virtio-pci` and the PCI ids,
- `device/subsystem` as a symlink to `/sys/bus/pci`,
- `device/drm` (libdrm's `drmNodeIsDRM` only stats it),
- `dev` = `226:0` / `226:128`.

Supporting that needed four kernel pieces: `open`/`read`/`fstat` for the
synthetic files, `readlinkat`, `newfstatat`/`faccessat` for them, and a
synthetic `/dev/dri` **directory** with `getdents64` (libdrm enumerates it).
`fstat` on `/dev/dri/*` now reports `S_IFCHR` with the right `st_rdev`, and
`faccessat` only claims the nodes that actually exist — the old blanket
`/dev exists` made libdrm classify `renderD128` as a primary node.

Result: `drmGetDevice2` returns 0 with `available_nodes = 0x5`
(primary|render), and Mesa gets past the device check.

`tests/linux/sysfstest.c` checks the tree directly (stat/open/read/readlink).

### Remaining: crash inside libgallium's screen setup

With the device accepted, Mesa creates the virgl context, fetches the
1408-byte virgl2 capset, and then dies with a translation fault:

```text
FAULT elr 0x411FD8B0 (libgallium +0x41d8b0) x1 0x44800
Page Fault at 0x40A04000 (ESR 0x92000047, write, translation fault L3)
```

`0x40A04000` is in no mmap region. The instruction is `stp s13, s12,
[x19, #8]` inside a loop that also does `str w1, [x20, #14712]` and
`ldr w1, [x22, #7440]` — it looks like a loop over a table whose pointer and
count come from a structure. Next step: print `x19`/`x20`/`x21`/`x22` and the
mmap regions at fault time, and check whether the table pointer is derived
from a capset value we return (e.g. a capability count or size the driver
expects to be smaller).

`egltest` (default build) and `sysfstest` pass; `drmtest`, `rendertest`
(GL device) and `layer-serve` stay green.

### virgl renders: `gl: renderer virgl (eForce GTX 1650/PCIe/SSE2)`

With the synthetic sysfs in place, `egltest` built with `-DEGLTEST_VIRGL`
brings the whole stack up:

```text
egl: version 1.4
gl: vendor   Mesa
gl: renderer virgl (eForce GTX 1650/PCIe/SSE2)
gl: version  OpenGL ES 2.0 Mesa 25.0.7-2+deb13u1
gl: framebuffer complete
```

So the render node, the virgl context, the 1408-byte capset, the host GL
translation and the FBO all work. `stdout` must be unbuffered in the test
(`setvbuf`), otherwise the earlier lines are lost when the process dies.

The remaining crash is the first `glClear`, after the FBO is complete and
after one `VIRTGPU_RESOURCE_CREATE`:

```text
FAULT elr 0x411FD8C8 (libgallium +0x41d8c8, ldr s2, [x19])
      x19 0x40A04004  x20 0x4099E700  x21 0x8148  x22 0x408F6700
Page Fault at 0x40A04004 (write/read, translation fault L3, no region)
```

`0x40A04004` is just past the 2 MiB heap window (`USER_HEAP_MAX_SIZE`);
raising `USER_HEAP_MAX_PAGES` to 4096 did not move the fault. The crashing
function starts at libgallium+0x41d6c0:

```asm
41d6c0  paciasp
41d6c4  stp  x29, x30, [sp, #-112]!
41d6d0  mov  x25, x0
41d6d4  cmp  w2, #0
41d6e8  add  x22, x3, w2, uxtw #4      ; end of an array of 16-byte entries
41d6f8  add  x23, x0, #0x18, lsl #12   ; x0 + 0x18000
...
41d8ac  stp  s15, s14, [x19]
41d8b0  stp  s13, s12, [x19, #8]
41d8bc  add  x19, x19, #0x20           ; 32-byte stride
41d8b4  ldr  w1, [x22, #7440]          ; loop bound, looks uninitialised
```

It copies float4s from a 16-byte-stride array into `x0 + 0x3b004` with a
32-byte stride, and the loop bound (`[x22, #7440]`) reads as ~33000 and
changes between runs, i.e. it is not the value the code expects. The
destination is inside a large heap object (likely `struct gl_context`), so
the loop runs past the end of the object.

Next step: find the caller of libgallium+0x41d6c0 (frame pointers are
unreliable through libEGL; a kernel-side stack walk of the user stack or a
`backtrace()` in the test would do it) and check which state field supplies
the bound. Prime suspects are the values we return from
`VIRTGPU_RESOURCE_CREATE` (`size`, `stride`, `res_handle`) and the caps
blob, since the crash follows the first resource creation.

`egltest` default build (swrast) still passes; `sysfstest`, `drmtest`,
`rendertest` (GL) and `layer-serve` stay green.

### virgl works: real GLES2 rendering through the GPU

`egltest` now renders and reads back the cleared colour on the host GPU:

```text
gl: renderer virgl (NVIDIA GeForce GTX 1650/PCIe/SSE2)
gl: version  OpenGL ES 3.2 Mesa 25.0.7-2+deb13u1
gl: framebuffer complete
gl: readback 64 127 191 255
egl test ok
```

Four kernel bugs stood between "context created" and pixels:

1. **`GET_CAPSET` data offset.** The response is
   `{ struct virtio_gpu_ctrl_hdr hdr; u8 capset_data[]; }` — the blob starts
   at offset 24, not 32. Reading at +32 shifted every capability by 8 bytes,
   so Mesa saw a garbage `MaxViewports` and a viewport loop ran off the end
   of `gl_context` (the "page fault just past the heap").
2. **`ctrl_hdr.ctx_id` is at offset 16**, not 4. It was written into `flags`,
   so `CTX_CREATE` made context 0 while `SUBMIT_3D` (fixed later) targeted
   context 1 and every command was silently ignored.
3. **3D resources need `ATTACH_BACKING`.** Without it the host has no guest
   memory to render into.
4. **Cache coherency.** BOs are mapped Normal Cacheable (so glibc's `DC ZVA`
   works), so `EXECBUFFER` cleans the BOs it references before submission and
   invalidates them after; `VIRTGPU_WAIT` invalidates the waited BO.

`SUBMIT_3D` also sets `VIRTIO_GPU_FLAG_FENCE`, which makes QEMU defer the
control response until the host finished executing the command buffer.

`egltest` now defaults to the virgl path (build with `-DEGLTEST_SWRAST` for
the software rasteriser); it needs `libgallium-25.0.7.so` preloaded so the
DRI stub can resolve `__driDriverGetExtensions_virtio_gpu`.

Next: run a GTK4 app with `GSK_RENDERER=gl`. That needs the compositor to
share buffers with the client (`zwp_linux_dmabuf_v1` or `wl_drm`), since GBM
surfaces belong to the render node, not to Wayland shm.

### Buffer sharing: PRIME/GEM and zwp_linux_dmabuf_v1

The kernel now exports render-node buffers:

- `GEM_FLINK` / `GEM_OPEN` give a buffer a global name and reopen it,
- `PRIME_HANDLE_TO_FD` turns a BO into an fd (`Target::Prime { pa, size }`)
  that any task can receive and `mmap` (the same frames),
- `PRIME_FD_TO_HANDLE` maps the fd back to the BO in the importer.

`tests/linux/primetest.c` covers the whole path: create a BO, write a
pattern, flink/open it, export the fd, map it twice, then fork and let the
child map the inherited fd and verify the pattern. It prints `prime test ok`.

The compositor advertises `zwp_linux_dmabuf_v1` v3 with ARGB8888/XRGB8888 on
the LINEAR modifier, handles `create_params`/`add`/`create`/`create_immed`,
and stores a dmabuf-backed `wl_buffer` in the same `BufferSlot` table the shm
path uses (the descriptor is owned by the buffer and closed on destroy).
GTK4 binds the global twice and the window composes.

Two compositor fixes came with it:

- the single-window blit wrote rows with the *client* width as the
  destination stride, so any window narrower than the output was garbled
  (the GTK4 window showed as diagonal stripes); it now uses the output stride
  and clips to the output size,
- `SERVE_GTK4` runs with `GSK_RENDERER=gl` and the Mesa environment
  (`LIBGL_DRIVERS_PATH`, `__EGL_VENDOR_LIBRARY_FILENAMES`, `LD_PRELOAD` of
  libgallium). It must **not** set `EGL_PLATFORM=surfaceless`: GTK4 needs the
  Wayland EGL platform.

`disk_gtk4.img` was resized to 512 MiB (`scripts/sfs_resize.py`) to hold Mesa
+ LLVM, and has Mesa 25, glvnd, `primtest`, `egltest` and the new
`gtk4-serve`. The GTK4 window renders correctly (title bar, buttons, label).

Still open: GSK currently presents through `wl_shm` — the client creates no
dmabuf buffer even though it binds the global, so it falls back to the cairo
renderer. Next step is to find why: GTK4 4.x asks for dmabuf *feedback*
(`zwp_linux_dmabuf_v1` v4 / `zwp_linux_dmabuf_feedback_v1`) and may refuse the
v3 path; either implement feedback or check `GDK_DEBUG=gl` output with
`G_MESSAGES_DEBUG=all` while the client still runs.


### GPU buffers end-to-end: GTK4 renders through GL

The dma-buf path is now complete enough that GTK4's GSK GL renderer works.

**Kernel: PRIME buffers carry pixels.** `PRIME_HANDLE_TO_FD` exports a BO as a
`Target::Prime { pa, size }` fd, and the compositor maps the same frames. Two
kernel bugs surfaced on the way:

- A BO's guest backing must be at least the level-0 layout. Mesa's GBM
  window-buffer path sends `size = 1` (the field is an input, not the kernel's
  output); taking it literally gave a 4 KiB backing for a 1024x1024 texture.
  `size = max(input, stride * height * depth * layers)`.
- virgl renders into a host GL texture, so the guest frames are stale until a
  `TRANSFER_FROM_HOST_3D`. Every BO referenced by a successful `EXECBUFFER` is
  marked dirty; `map_prime()` (a compositor mapping a PRIME fd) copies the
  resource back into the frames first. `primetest` still passes because a BO
  that was never submitted is not dirty and its CPU-written pattern survives.

`SUBMIT_3D` now accepts 256 KiB (Mesa's `VIRGL_MAX_CMDBUF_DWORDS` is 64K
dwords) and the whole control queue runs under a spin lock; before, a large
shader buffer was rejected and the host's long-shader decoder lost its state
("Got continuation without original long shader").

**Compositor: `zwp_linux_dmabuf_v1` v4 + feedback.** The format table advertises
ARGB8888/XRGB8888 with both LINEAR and `DRM_FORMAT_MOD_INVALID`. The INVALID
entry is essential: Mesa 25's `create_dri_image()` returns without allocating
when the driver lacks `resource_create_with_modifiers` (virgl) and the
compositor does not list INVALID as an implicit-modifier option. Without it
`eglMakeCurrent` reports `EGL_BAD_ALLOC: failed to allocate color buffer` and
`eglSwapBuffers` crashes in `dri2_query_image(NULL)`. With it GSK binds a real
dmabuf-backed framebuffer (`SET_FRAMEBUFFER_STATE` now appears in the virgl
stream) instead of drawing to the incomplete default framebuffer of QEMU's
`egl-headless` context.

`tests/linux/wlgltest.c` isolates this path: a real `wl_egl_window`, two
`glClear` colours and two `eglSwapBuffers` calls, run with `linuxstart wlgl`
(SERVE_WLGL). It prints `wlgl: renderer virgl (...)` and `wlgl test ok`; the
compositor logs `gtk-serve: dmabuf 256 256 1024 ...` and the sink shows the
second frame's green, so the per-frame host->guest copy works too.

**DRM version 0.0, not 0.1.** The second swap initially hung in Mesa with no
syscalls and no protocol traffic. `DRM_IOCTL_VERSION` reported minor 1, which
Mesa's virgl winsys reads as "the kernel implements
`VIRTGPU_EXECBUF_FENCE_FD_OUT`". This kernel submits synchronously and returns
no fence fd, so Mesa polled fd -1 forever in `sync_wait()`. Reporting 0.0 makes
Mesa use the legacy `VIRTGPU_WAIT` path (which completes immediately here).
Only `supports_fences` depends on that version.

With that, `linuxstart gtk4` (SERVE_GTK4, `GSK_RENDERER=gl`) renders the whole
window through virgl: title bar, buttons, antialiased label, presented as a
1080x826 dmabuf. `egltest` (offscreen FBO + readback) and `primetest` stay
green. Phosh's cairo/shm path still shows the pre-existing red-block/black
frame; it reproduces with the HEAD kernel and the previous compositor, so it is
not caused by these changes.


### Phosh renders again: gdk-pixbuf without glycin

After the GPU work the Phosh image stopped at GTK icon loading: the lock
screen never appeared and Phosh aborted after 2.5 minutes with

```
Gtk:ERROR: gtkiconhelper.c:495: ensure_surface_for_gicon: assertion failed
(error == NULL): Failed to load /org/gtk/libgtk/icons/48x48/status/image-missing.png:
Operation did not complete in supplied limit of 60s
```

The 60 s limit is glycin's. The Arch aarch64 gdk-pixbuf (2.44) has no
built-in loaders; every decode goes through glycin, and its gdk-pixbuf
integration either reports `Failed to get the filename` or hangs waiting for
the loader reply. `glycintest` itself still decodes a PNG, so the loader binary
is fine; the failure is in the gdk-pixbuf integration, which also blocks the
wallpaper.

Fix: install Debian 13's gdk-pixbuf 2.42.12, whose PNG/JPEG loaders are
compiled into `libgdk_pixbuf-2.0.so.0`. It keeps the same soname and every
`gdk_pixbuf_*` symbol imported by the guest's GTK4/phosh is exported by it.
The Debian build needs `libjpeg.so.62` (Arch ships `libjpeg.so.8`), so the
matching `libjpeg62-turbo` goes into `/sfs` too. No loader modules or
`loaders.cache` are involved. `tools/install_gdk_pixbuf.py` downloads both
`.debs`, extracts the libraries and injects them (idempotent; `--cache` keeps
the downloads).

With that, `pixbuftest` prints `pixbuf 64x64` (it used to fail with
`Failed to get the filename`) and `linuxstart phosh` shows the lockscreen:
night-sky wallpaper, the big clock (updating), "Thursday, January 1". The top
~60 px strip stays black (Phosh's panel layer is empty on the lockscreen) and
the unlock prompt below the date is clipped by the 1024x768 output. The
compositor now also logs `gtk-serve: waiting for client` / `client connected`
so a silent compositor is easy to tell from a slow client.

Remaining Phosh work: the panel/quick-settings (the QS panel still never
appears), logind/polkit sessions, and the empty top strip.


### Desktop: 1024x768 logical output, unlock, and a working quick-settings panel

The Phosh lock screen is a GtkWindow whose content has a minimum height of 658
logical pixels (clock, date, PIN carousel). At `wl_output.scale=2` the logical
screen was 512x384, so GTK clamped the window to 658 and the unlock keypad and
prompt sat below the scanout. GTK ignores a layer-surface configure smaller than
the content minimum, so no amount of configure massaging helps: the desktop
itself has to be big enough.

`wayland_real.c` now has an `OUTPUT_SCALE` knob (currently **1**). The compositor
advertises the 1024x768 scanout as a 1024x768 logical desktop at scale 1, and
`LOGICAL_WIDTH`/`LOGICAL_HEIGHT`, the pointer coordinate conversion, the input
region scaling and the fractional-scale value all derive from it. The lock
screen, the "Slide up to unlock" hint and the quick-settings sheet fit.

Unlocking needs authentication (`sm.puri.phosh.lockscreen require-unlock`
defaults to true) but the image has `/etc/passwd` only: no `/etc/shadow`, no
`/etc/pam.d`, so `pam_start("phosh")` can never succeed. The compositor already
seeds a GLib keyfile backend for the desktop defaults; it now also writes
`require-unlock=false`, so a key press or a swipe unlocks. Real PAM/logind
support is still the prerequisite for a password-protected session.

The top panel is a `zphoc_draggable_layer_surface_v1` client: the compositor
owns the gesture and moves the surface by rewriting one layer margin.
Implemented here:

- `get_draggable_layer_surface` plus `set_margins`, `set_threshold`,
  `set_exclusive`, `set_drag_mode`, `set_drag_handle`, `set_state`, `destroy`.
- The folded margin is applied on commit and after every `set_margins`; a drag
  adjusts it between folded and unfolded, emits `dragged(margin)` on every
  change and `drag_end(state)` on release (threshold decides the state).
- Top/bottom anchored surfaces drag vertically, left/right horizontally. The
  first cut used `x` for the top panel, which made every drag a no-op.
- Phosh computes its folded margin and drag handle from the height it
  configured (768), but GTK sizes the panel surface to its content (252), so
  the folded margin is clamped to "only the 32px bar visible". The handle is
  not enforced: the whole visible panel drags.

Quick settings now opens by swiping down from the top bar and folds on the way
up; the same margin machinery is what Phosh uses to fold the panel after
unlocking.

Two rendering fixes came out of the same work:

- **Premultiplied alpha.** `layer_compose` blended nothing: any pixel with a
  non-zero alpha was written as-is, which turned Phosh's translucent home and
  panel-backdrop layers black. It now blends `src + dst * (1 - a)` and honours
  `zphoc_alpha_layer_surface_v1.set_alpha` (a per-surface table; previously one
  global id).
- **Per-layer pixel snapshots.** GTK destroys the `wl_shm` pool as soon as the
  compositor releases the buffer, so re-mapping the client fd at composite time
  made static layers (wallpaper, home) disappear after any other surface
  committed. Each layer entry keeps its own copy in anonymous memory, taken
  while the pool is still alive.

And one protocol bug that made input erratic: `wl_buffer.release` was sent on
*every* commit, including commits without `attach`, so the same buffer was
released twice; GDK's cairo renderer destroys the buffer's cairo surface on the
first release and aborted with a `cairo_surface_reference` assertion at unlock.
Release now happens once per fresh attach.

The input ring consumer wrapped its tail modulo the slot count while the host's
head is monotonic. Once `head` passed 64, `tail != head` stayed true forever and
the compositor replayed the whole ring instead of consuming it (duplicated
clicks, stuck gestures). The tail is monotonic now, like the head.

Tests: `cargo test --lib` 222/222; `layer-serve` and `linux-wlreal` green (the
test image needed a current `wlclient`); `sysfstest`, `primetest` and `egltest`
green (`readback 64 127 191 255`). `wlclient` now expects the scale-1 output:
logical size 1024x768, `wl_output.scale=1`, fractional 1.0 and the pointer
entering at output (200,100).

Remaining Phosh polish: the folded top bar is an empty 32px strip (no clock or
status icons there), the quick-settings sheet is black because the blurred
backdrop layer does not render, and the first tile row (Cellular/Wi-Fi/
Bluetooth) loses its labels.


### A terminal application, and executable bits for SFS

Phosh's image is GTK3 + libhandy (the GTK4 stack lives only in the GTK4 test
image), so the first real application is a GTK3 terminal:
`tests/linux/term.c`. It keeps a text view, runs each command through
`/sfs/busybox sh -c <line>` with `g_spawn_sync`, appends stdout/stderr and the
exit status, and supports `cd` (tracked in the prompt) and `clear`. The
compositor gained a `SERVE_TERM` mode that forks `/sfs/term`, so
`linuxstart term` runs it end to end. Verified: `uname -a` prints
`Linux werewolf 0.1.1 #1 SMP aarch64 GNU/Linux`, `ls /sfs` lists the image.

`tools/install_terminal.py` installs the pieces reproducibly: the static
Debian busybox (`/busybox`), the terminal (`/term`), a launcher entry
(`/share/applications/werewolf-terminal.desktop`) and the hicolor icon.

**Keyboard release.** The input ring only carried a key press, so the
compositor sent `wl_keyboard.key` with pressed=1 and never a release. GTK
implements key repeat itself from `wl_keyboard.repeat_info`, so a key that
never goes up kept activating: pressing Enter in the terminal ran the command
four times. The compositor now follows every press with a release.

**Executable bits (kernel).** `g_app_info_get_all()` returned no applications
even though the `.desktop` file parsed and `stat` worked: glib's
`g_desktop_app_info_load_from_keyfile()` validates `Exec` through
`g_find_program_for_path()`, which needs an executable file, and SFS inodes
carry no permission bits (everything was reported as `0644`). Linux `stat` and
`fstat` now report SFS regular files as `0555` (read-only image, everything
runnable) while the writable `/tmp` tree stays `0644`. With that,
`g_app_info_get_all()` returns `werewolf-terminal.desktop | Terminal` and the
file/executable checks used by shells and toolkits behave.

**Global shortcuts.** Phosh grabs its shell accelerators (Super for the
overview, Print, Alt+F2, ...) through `phosh_private_keyboard_event`: the
compositor must acknowledge `grab_accelerator_request` and deliver
`accelerator_activated_event`. `wayland_real.c` now keeps a grab table, maps
the accelerator name to an evdev keycode (Super, F1-F12, Print, letters) and
forwards activations, with ungrab support. Phosh does not currently create the
keyboard-event object in this session (no `get_keyboard_event` request is
seen), which is why the overview cannot be opened yet.

**Next: multi-client compositing.** The compositor still serves exactly one
Wayland client, so Phosh cannot launch applications: a second `accept()` never
happens. Making the server multi-client is the prerequisite for the app grid
and for any real session; the per-client state save/restore used by the
multi-client test is the starting point, but surface/buffer/pool tables have to
become per-client because object ids collide across clients.


### Multi-client compositor (first cut)

The compositor served exactly one Wayland client, so the shell could not start
anything: a second `accept()` never happened. It now accepts up to four
clients and round-robins them with a single `ppoll()` over the listening
socket and every client fd.

Each client keeps its own protocol state in `ClientState` (objects, pools,
buffers, regions, seat proxies, focus, clipboard). Two things are global
instead:

- **The layer scene.** `layer_entries` is no longer swapped with the client
  state: Phosh's panel/background/home have to stay composed while an
  application is being served. Entries remember their `owner` so a new client
  only clears its own.
- **The application window.** The newest `xdg_toplevel` is copied into
  `app_window` (same reason as the layers: GTK destroys the shm pool when the
  buffer is released) and drawn over the layer scene with the alpha blend. One
  window is enough until there is a window manager.

A partially received message must not be spliced onto another client's bytes,
so the pending tail (up to 4 KiB) is part of `ClientState` and restored on the
next serve of that client. New clients are initialised with
`init_client_state()`, which zeroes their state *without* closing descriptors
the other clients still own — the old `reset_client()` at accept closed the
previous client's shm pools and dmabuf fds.

Input goes to the application window when one exists (the terminal is
full-screen there), otherwise to the client that owns the layer scene.

Verified: `cargo test --lib` 222/222, `layer-serve` and `linux-wlreal` green,
and the Phosh flow still works (lockscreen, Space to unlock, swipe opens and
folds quick settings).

**Not finished.** A launched application connects, binds globals (compositor,
shm, seat, xdg_wm_base, ...) and completes two roundtrips, then stalls before
creating its surface; no commit ever arrives, so nothing is drawn. The
multi-client plumbing itself is exercised (the second client is served and its
state is preserved), but GTK's startup blocks somewhere after the second
`wl_display.sync`. Debugging this needs a `WAYLAND_DEBUG` log from the client
side (Phosh runs with it off nowadays) to see which request it is missing.


### Per-client receive buffers

Switching sockets between clients could splice one client's partially received
message onto another client's bytes: the receive buffer was global and only a
4 KiB tail was carried in `ClientState`. Each client now owns the full 64 KiB
buffer and `load_client()` simply points the receive path at it.

Verified: `layer-serve`, `linux-wlreal` and the Phosh flow (lockscreen,
unlock, quick settings open/fold/open) are green with the new buffer.

**Still open:** a GTK application launched as the second client stalls after
the first `wl_display_roundtrip` — it receives all globals and the sync `done`
(confirmed with an in-shmem protocol trace) but sends no further requests,
while a plain libwayland client as the second client completes the whole
sequence. The behaviour is nondeterministic between runs, which points at the
multi-client state switching rather than a missing protocol event; the buffer
fix above did not change it.


### Input servicing only when the host queued something

`poll_input()` was called on every loop iteration, loading and saving a
client's protocol state each time. That runs the whole `load_client` /
`save_client` copy (pools, buffers, regions, seat proxies, the 64 KiB receive
buffer pointer) 50 times a second with nothing to do, and a second client
(GTK, in this case) never got past its first roundtrip. Gating the call on the
input ring's head/tail fixes the symptom: with no host input the state is no
longer churned, and the terminal now binds globals, creates surfaces and
requests frames (confirmed with the in-shmem protocol trace).

The Phosh flow still works with the gate (lockscreen, Space unlock, quick
settings open/fold/open).

**Still open:** the terminal's window is not visible after it launches. The
protocol trace shows it binding and creating surfaces, so the next question is
whether its `wl_surface.commit` reaches the `app_window` capture path at all
(trace `object, opcode == 6` for client 1) — the capture requires
`layer_slot == 0`, so a stale layer-surface match is the first suspect.


### Surface identifiers are per client

The launched terminal drew its first frame and then closed the connection
120 ms later with `Gdk-Message: Error reading events from display: Invalid
argument` and exit status 1. `WAYLAND_DEBUG=1` showed the last exchange was
`wl_surface.commit()`, and GDK3's event source `_exit(1)`s as soon as
`wl_display_read_events()` fails.

The failure was a protocol error on the *client* side, not a broken socket:
the compositor had just handed the application a `wl_pointer.enter` /
`wl_keyboard.enter` whose surface argument was the *shell's* layer-surface id.
Surface ids live in the owning client's namespace, so that id either names an
unrelated object or nothing at all; libwayland-client logs
`unknown object (N), message wl_pointer_enter(...)`, sets `errno = EINVAL`
and remembers it in `display->last_error`. GDK only notices on the next read,
which is why the message points at "reading events" and why the kernel's
`recvmsg`/`ppoll` never failed.

`pointer_route()`, `keyboard_route()` and `drag_begin()` now check
`LayerEntry::owner` before targeting a surface, and `pointer_route()` also
hit-tests the newest toplevel for its own client, so an application window is
clickable instead of falling through to "no surface here".

Two follow-ups ride along:

* `xdg_surface.set_window_geometry()` is recorded and used when compositing
  and hit-testing the toplevel. A GTK window with client-side decorations
  arrives as a 1076x820 buffer whose content starts at (26, 23); anchoring the
  *content* at the output origin makes a 1024x768 terminal fit exactly instead
  of spilling 52 px off the bottom edge.
* F1 toggles the newest toplevel (and launches the terminal when none is
  mapped). Without a window manager this is the way back to the shell: the
  compositor still routes input to the application while its window is up.

Verified: `cargo test --lib` (222), `layer-serve`, `linux-wlreal`, the Phosh
flow (lockscreen, unlock, quick settings) and, as the second client, the GTK3
terminal — it stays alive, composes over the desktop and runs commands
(`ls /sfs` printed the SFS listing in the text view).


### Publishing frames to the host viewer (WIN1 seqlock)

The compositor wrote a constant frame counter (`screen_seq * 2 + 1`, and
`screen_seq` was never incremented) and never set `STATE_READY`. The host
viewer implements the seqlock from `userspace/src/window_proto.rs`: it copies
a frame only when the counter is even and unchanged across the copy. A
constant odd value therefore meant **the window never updated at all**, and a
harness could not tell a fresh frame from a stale one.

`layer_compose()` now publishes the way Wolfser does: bump the counter (odd)
before painting, write geometry and `STATE_READY`, then bump it again (even)
after the pixels are visible. The Phosh E2E grabs now report increasing even
sequences (2, 276, 288, ...) instead of a constant 1.

### Cursors: no theme at all

GDK loads cursor images from an Xcursor theme and `wl_cursor_theme_load()`
only searches `XCURSOR_PATH` — not `XDG_DATA_DIRS`. The image had no cursor
theme, so every pointer enter logged `Unable to load <name> from the cursor
theme` (49 lines in one run, and the same flood from Phosh filled the serial
console, which is what the user saw on the host).

`tools/install_cursor_theme.py` generates a minimal theme (arrow, I-beam,
crosshair, ring at 24x24, drawn from ASCII masks with a white outline) and
installs `/share/icons/default/cursors/*` plus `index.theme`; the shell and
the terminal get `XCURSOR_THEME=default` and `XCURSOR_PATH=/sfs/share/icons`.
The flood is gone (0 lines).

`NO_AT_BRIDGE=1` stops GTK from loading atk-bridge: with no accessibility bus
the D-Bus activation sat on its timeout for ~30 s before `activate` fired.

### Application startup: 50 s -> 16 s

Measured with a throwaway probe (`gdk_display_open`, then `gtk_init`, then
`gtk_window_new`, then `show_all`) and a temporary slow-syscall log in the
kernel (`dispatch()` timing, since removed):

* `gdk_display_open` 0.4 s, `gtk_init` 0.1 s — connecting is cheap.
* `gtk_window_new` took **30 s**: the first window makes GDK touch GL, whose
  `libEGL_mesa` pulls `libgallium` -> `libLLVM` -> `libz3`. Both LLVM copies
  (117 MiB and 163 MiB) were mapped a page at a time.
* The mapping itself is the cost: `mmap` of a file segment copies every page
  through `virtio-blk`, and the driver issues **one 512-byte request per
  sector** (~15 MB/s). 122 MiB took 7.7 s inside `pread`, 0.6 s elsewhere.

Two fixes:

1. `mmap_file_paged()` reads the file in 64 KiB runs instead of 4 KiB pages
   (16x fewer requests, ~25% faster on its own).
2. `tools/prune_software_gl.py` removes `libLLVM*` and `libgallium*` from the
   desktop image. Phosh and the terminal paint through `wl_shm`; the software
   GL stack (llvmpipe) is only used by GL clients, which live in
   `disk_gtk4.img`. This frees ~400 MiB and removes ~25 s of copying.

Terminal: `client connected` -> first frame is now ~16 s (was ~50 s), with no
cursor messages and the window staying alive. The remaining cost is the
remaining library mappings (libicudata 33 MiB, libgtk 8.5 MiB) and font/CSS
work at `show_all`; the real fix there is lazy file-backed mappings, which
the kernel does not have yet (`lazy` regions are anonymous-only).

Verified: `cargo test --lib` (222), `layer-serve`, `linux-wlreal`, the Phosh
flow (lockscreen, unlock, quick settings) and the terminal as the second
client.


### File-backed mappings are lazy

`mmap` of a file copied the whole requested range into fresh frames before
returning, so the dynamic loader's `PT_LOAD` segments were paid for up front:
libLLVM's 122 MiB cost 7.7 s of `pread` (the block driver issues one 512-byte
request per sector, ~15 MB/s) and several hundred MiB of DRAM that the program
never executes.

`MmapRegion` now carries `file: Option<(path, byte offset)>`. A mapping of a
filesystem file is `lazy`: `lazy_page_fault()` allocates one frame, reads that
page through `vfs::read_path_at()` and only then maps it. The path is a stable
reference because the SFS image is immutable; a file removed from a writable
mount turns the fault into a task kill rather than a stale read. `memfd` and
other synthetic descriptors keep the eager path (they have no path).

Two follow-ups were needed:

* `split_region_at()` clones a region when `mprotect` cuts it in half. The
  tail's page 0 is `offset_pages` further into the file, so its file offset is
  advanced by `offset_pages * PAGE_SIZE`. Without that, a RELRO `mprotect` on
  part of a library segment would make the tail read the wrong pages.
* The kernel copy path validates the page table *before* touching user memory
  (`PAN` is on and `copy_from_user`/`copy_to_user` use `ldtrb`/`sttrb`). An
  untouched lazy page therefore failed validation and the syscall returned
  `EFAULT` without the hardware ever faulting the page in — dbus-daemon died
  with `Failed to open /dev/null: Bad address` because its path literal lives
  in a lazily mapped `.rodata` page. `user_memory` now backs the pages of the
  window (`back_user_pages`) with a read or write `cow_fault` before retrying
  the validation; `copy_to_user` already did this for fork-COW pages.

Measured (`linuxstart phosh` → first composed frame, and F1 → terminal window):

| | before | after |
|---|---|---|
| Phosh first frame | ~139 s | **10 s** |
| F1 → terminal window | ~45-60 s | **6 s** |
| terminal connect → first frame | ~50 s | **3.4 s** |

`Phosh ready after 7.08s` in its own log. The terminal still runs commands
(`ls /sfs` prints the listing), and the shell's lockscreen/quick settings flow
is unchanged.

Verified: `cargo test --lib` (222), `layer-serve`, `linux-wlreal`, the Phosh
flow (lockscreen, unlock, quick settings open/fold/open) and the terminal as
the second client.


### App grid / overview: what Phosh 0.57 needs

Super does not open the app grid yet, so the terminal is still launched with
F1. What the investigation established:

* Phosh's home (overview + app grid) is a `PhoshHome` `GtkWindow` built on
  `PhoshDragSurface`, driven by `zphoc_draggable_layer_surface_v1`; its
  visibility is bound to `PhoshShell::overview-visible` (TRUE from startup).
* A layer surface must be configured before GTK commits a buffer. Phosh only
  calls `set_size` for surfaces it resizes, so the "fill the output" home
  surface was never configured; `get_layer_surface` now sends an initial
  `configure(serial, w, h)` and Phosh reports
  `Mapped 'phosh home'` / `Configured 'phosh home' (1024x768)`.
* Super reaches Phosh: Phosh registers `Super_L`/`Super_R` through
  `phosh_private_keyboard_event.grab_accelerator_request`, our `grab_activate`
  fires action 9, and Phosh runs `toggle_overview_action`.
* `zphoc_draggable_layer_surface_v1.set_state` must be answered with **both**
  `dragged(margin)` and `drag_end(state)`; the client only leaves its folded
  state on those events. Implemented: Phosh now logs
  `DragSurface ... state, 2` then `1` (dragged -> unfolded).
* `zphoc_alpha_layer_surface_v1.set_alpha` has to update the *stored* layer
  entry and recompose. Phosh fades the quick-settings backdrop and the home
  background after the surface committed, so a late alpha 0 was ignored and
  an opaque backdrop covered the whole screen.

**Still open:** with the home unfolded, mapped and configured, its surface
commits `attach(NULL)` (empty frames) and nothing is drawn. Phosh's own log
shows the same `Configured 'phosh home' (1024x768)` twice and no visual. The
next question is why GTK does not paint that window: the overview widget tree
may be empty (no launcher entries), or the window needs an enter/frame clock
push our compositor does not provide for that surface. Until then F1 remains
the launcher: it shows/hides the newest toplevel and starts the terminal when
nothing is mapped.


### Host input: ring positions, key values, phantom window

Running the desktop in `tools/window-viewer` showed the lockscreen but the
mouse did nothing. Three separate causes:

* **The input ring desynchronised.** `window_proto.rs` says both fields hold
  ring positions (`0..63`) and the viewer writes `(head + 1) % slots`, while
  `poll_input()` kept its own *unbounded* tail. The two agreed only until the
  first wrap; after that every event was read from a stale slot, so the guest
  kept replaying old entries - "the mouse stopped working" after ~64 moves.
  Both sides are modulo now (`pending = (head + slots - tail) % slots`).
* **The viewer speaks Unicode, the clients speak evdev keycodes.** Printable
  keys arrive as Unicode code points (case preserved) and the rest as the
  small `KEY_*` values; the compositor now translates them (`keycode_for`)
  and brackets shifted characters with a `wl_keyboard.modifiers` mask so GTK
  produces the right character. The viewer cannot send F1 at all, so the
  launcher shortcut moved to **Ctrl+T** (`KEY_NEW_TAB`), which it can send.
* **A phantom application window.** Phosh creates a couple of toplevels while
  it starts; the compositor captured the first one as the overlay window, so
  it drew over the desktop and Ctrl+T *hid* it instead of launching. Only
  clients other than the shell (`current_client != ui_client`) own the
  overlay window now.

Verified with viewer-style events (ring positions, Unicode keys): 90 hover
moves, then a swipe unlocks; Ctrl+T starts the terminal; typing `ls /sfs`
prints the SFS listing in it. `layer-serve`, `linux-wlreal` and the Phosh
quick-settings flow stay green.


### Frame cost: what was actually slow

The desktop ran, but slowly, and the serial console was flooded with
`layer compose px=...`. Measured with a temporary per-frame timer (100-frame
averages, QEMU TCG):

| | before | now |
|---|---|---|
| compose | 30-64 ms | **27 ms** |
| of which blit | 24-61 ms | 24 ms |
| of which GPU head | 5 ms | 1 ms |
| composes for 10 s of lockscreen | ~250 | ~260 (Phosh's own rate) |

What changed:

* The per-frame console line is gone. It cost a UART write per composed frame
  and was the "spam" in the terminal; nothing else prints per frame.
* `layer_compose()` is no longer called from every commit. Phosh commits its
  background, the lockscreen and the panel in the same frame, so composing
  after each of them meant three full-frame blits per visible frame. A
  `scene_dirty` flag now composes once per main-loop iteration.
* Layer pixels are stored **pre-swizzled** (BGRA -> RGBA, XRGB forced opaque)
  in `layer_store_pixels`/the app-window capture, so the compose pass only
  copies or blends words. A static layer (the wallpaper) is no longer
  swizzled again on every composed frame.
* The blit hoists the per-layer alpha: an opaque layer takes a word-copy path
  with no per-pixel multiply/divide.
* The virtio-gpu head is refreshed every fourth frame (the ivshmem sink is
  what the window viewer reads) and its copy is 64-bit word-wise.

Frame latency end to end from viewer-style input: swipe -> unlocked in 0.53 s
(including the injected event delays), Ctrl+T -> terminal in ~5 s, typing
`ls` runs it.

**Still open:** the compose is a full-screen redraw every frame (~24 ms of
pixel work under TCG). Per-layer damage tracking (compose only the rects that
changed, caching the rest) is the next big win; the shell's own GTK redraw
cost is on top of that.


### Unlock, invisible blockers, and damage tracking

Two things surfaced when the desktop was driven from the window viewer:

* **A transparent surface swallowed the pointer.** Phosh's
  `top-panel background` (the quick-settings backdrop) is a full-output layer
  surface at an overlay layer. While the sheet is folded Phosh fades it to
  alpha 0, so it is invisible - but the hit test still picked it, and the
  lockscreen never saw the mouse. `layer_hit_test()` now skips entries with
  `alpha == 0` (and entries with no stored pixels). A click or any key on the
  lockscreen then unlocks it (Phosh's `require-unlock=false` path); measured
  0.25 s from click to the unlocked frame.
* **The sheet itself is opaque black.** We cannot blur, so the quick-settings
  backdrop renders as black over most of the screen. That is a cosmetic
  limitation, not a blocker: dragging it back up folds it.

Frame cost work (QEMU TCG, 1024x768):

* `wl_surface.damage` / `damage_buffer` are tracked per surface, and
  `layer_store_pixels()` re-reads only the damaged rows/columns. A clock tick
  or a keystroke no longer re-swizzles a full-screen buffer; a typed line in
  the terminal now costs a few hundred pixels.
* `layer_compose()` honours a damage box: it clears and repaints only the
  union of the changed rectangles (the whole frame when a surface moves,
  appears, disappears, or changes alpha). A small update costs ~1 ms instead
  of 27 ms; the full-frame path is unchanged.

The static-lockscreen case still composes a full frame per Phosh redraw
(~24 ms), so the next step for 30-60 fps is to find out why Phosh redraws at
all when nothing changed (its frame clock gets a `wl_callback.done` at our
50 Hz idle tick).


### Damage by content, not by what the client claims

Measuring the frame cost showed the partial composes were still full-frame:
`wl_surface.damage` is useless here because GTK damages its **whole window**
on every redraw, and Phosh redraws its lockscreen ~25 times a second. Every
one of those frames therefore re-read and re-painted 1024x768 pixels.

`layer_store_pixels()` now compares the new buffer with the stored copy 64
bits at a time and returns the **bounding box of the pixels that actually
differ**. Only that box is copied and only that box is composed. A clock tick
or a pulsing hint becomes a ~200 pixel repaint:

| | before | now |
|---|---|---|
| compose (changed frame) | 24 ms | < 1 ms |
| compose area | 786432 px | ~200 px |
| composes per second (lockscreen) | ~26 | ~40 |

A surface that appears, moves, changes layer order or alpha still forces a
full repaint (`layer_record_commit` watches those fields, and
`layer_forget_surface`/drag/alpha set `compose_full`).

End to end: Phosh's first frame 9 s, click-to-unlock 0.22 s, Ctrl+T ->
terminal ~9 s, `ls` runs. The cost left per frame is the compare scan
(~3 MB, the price of knowing what changed) plus Phosh's own GTK redraw.


### Where the app grid stands

State after this round (all committed):

* The home bar **is** drawn: a ~15 px black strip at the very bottom of the
  unlocked desktop is `phosh home` in its folded position (margin -753).
* A **click** on that bar never reaches Phosh: the press matches
  `drag_begin()` because the home is a `PhoshDragSurface`, so the compositor
  claims it as a drag of zero distance and `drag_finish()` settles it back to
  folded. Swiping it up (762 -> 280, 20 steps) also leaves the scene
  unchanged, even though the layer surface is now configured (which is what
  creates the draggable object) and `set_state` is answered.
* `wl_surface.enter` is now sent **per surface** instead of once per client.
  GDK tracks which outputs a window is on; only the first surface used to get
  the event. It did not change the home's behaviour by itself.

Next suspects for "the home unfolds but paints nothing": whether Phosh's
`PhoshDragSurface` drag threshold (`PHOSH_HOME_DRAG_THRESHOLD` 0.3 of the
748 px span) is reached in the drag, whether Phosh's home window is `mapped`
in GTK terms (`phosh-layer-surface-DEBUG` says "Mapped 'phosh home'"), and why
its first frames are `attach(NULL)` - GDK may treat a layer-shell window that
never got an `xdg`-style configure cycle as not drawable. Running Phosh with
`G_MESSAGES_DEBUG=all` and reading `phosh-home`/`phosh-drag-surface` debug
lines is the cheapest way to see whether the state change happens at all.


### Swipes on layer surfaces, and two Phosh findings

* While a button is held on a **layer** surface the compositor now keeps the
  pointer still: Phosh's gestures are touch gestures (its lockscreen only
  unlocks on a swipe `is_touch`), so forwarding the motion turned every mouse
  swipe into a drag that cancelled the click. A press plus a release at the
  same place is a click, which unlocks (require-unlock is off) and presses
  panel buttons. The application window still gets real motion for selections
  and scrolling. The quick-settings drag is unaffected: it is a
  compositor-side drag (`drag_begin`), not a pointer motion stream.
* A scene dump written into the ivshmem window (the serial console shreds
  multi-line output when Phosh also logs) showed, the moment Phosh starts:
  * the home/overview **is unfolded** (`state 1`, `current 0`, margin 0) with
    `drag_mode = NONE` (Phosh disables dragging while unfolded), so a swipe on
    the home bar cannot grab it - a click toggles it instead;
  * the home surface (74/75) **never commits a buffer**: Phosh maps it,
    configures it, logs `Mapped/Configured 'phosh home'`, and then only sends
    `attach(NULL)`. Nothing paints, so the app grid stays invisible even
    though the state machine says it is open. Only the lockscreen (43) and the
    panel/backdrop (62/69) ever draw.
* The panel/quick-settings layer surface sometimes ends up configured
  **1024x768** instead of its content height (252 or 32). `set_size(0, 0)`
  means "let me size myself", but our configure always proposes the whole
  output and GTK adopts it, so the folded sheet covers half the screen and the
  open one covers everything - the "шторка пол экрана" symptom. Next: propose
  the output size only for axes that are anchored on both sides and keep the
  client's own size on the free axis.


### The app grid paints: multi-descriptor SCM_RIGHTS

The home / app grid ("phosh home", surface 74/75) was mapped, configured,
unfolded, and committing 1024x768 frames - and the compositor still drew
nothing. The frames were real; the compositor silently dropped their buffers.

`libwayland` flushes every pending message of a connection in one sendmsg, so
several `wl_shm.create_pool` calls can arrive in a single cmsg array with
several descriptors. The receive path kept only the first descriptor
(`incoming_fd`), the later pools were never registered, and every buffer
created from them (`wl_shm_pool.create_buffer`) failed the pool lookup and was
never added to the buffer table. The commit then found no buffer and skipped
the surface: no layer entry, no pixels, no input. This affected exactly the
windows Phosh creates in one go - the home, its wallpaper background and the
top-panel backdrop - while the lockscreen and panel (each with its own pool in
its own flush) worked.

`receive_message` now parses the whole cmsg array into a small descriptor FIFO
and `take_incoming_fd()` pops one per fd-carrying request (create_pool,
dma-buf add, data_offer.receive, gamma). After that the grid, its wallpaper
and the backdrops all reach the buffer table.

Two follow-ups that the visible scene needed:

* Phosh only sends `set_alpha` to the quick-settings backdrop when the panel
  folds or unfolds, and it never folds at startup (the shell folds the panel
  before the panel exists). At alpha 1 the backdrop is an opaque sheet over
  the whole desktop, which read as "everything is black". A layer surface
  that was never given an explicit alpha is now transparent while the drag
  surface it is stacked below is folded (`layer_effective_alpha`).
* The folded margin of the top panel is clamped no more: Phosh computes it
  from the surface height and its layout puts the bar at the bottom of a
  full-height surface, so an old, smaller buffer height made the "clamp to
  32 px visible" rule leave the sheet half-way down the screen.

Verified end to end: lock screen renders (clock, date, "Slide up to unlock"),
Space unlocks (require-unlock is off), the overview shows the wallpaper, the
search entry and "Show All Apps", Ctrl+T opens the terminal and `ls` runs.


### A real window: floating, movable, closable

The application toplevel is no longer a full-output blit. The compositor
configures it to 780x520, places its content rectangle in the middle of the
output and draws the client's CSD margins around it, so the terminal is a
window over the desktop instead of a second full screen. The compositor owns
two pieces of chrome on top of the client's title bar: a close button in the
top-right corner (sends `xdg_toplevel.close`, then hides the surface) and a
title-bar drag that moves the window. The press/release of a chrome action is
swallowed so the client never sees half a click.

`zwlr_foreign_toplevel_manager_v1` is now implemented for that window:
`toplevel`, `title`, `app_id`, `output_enter`, `done` when the first frame
arrives, `closed` when the client goes away, and Phosh's `close` request is
forwarded to the app. Two details mattered:

* Phosh's manager object lives in Phosh's object namespace, so the announce
  reads `clients[ui_client].foreign_toplevel_manager_id`, not the global
  (which holds the *app's* state while its message is processed).
* handles created by the compositor must use ids above `WL_SERVER_ID_START`
  (`0xFF000000`). A low id made libwayland abort Phosh's connection with
  "Error reading events from display: Invalid argument".

Also: the app window is tracked by the toplevel's `wl_surface` only. Clients
commit cursor surfaces too, and capturing one shrank the window to 24x24.

And the drag-surface bookkeeping learned not to answer `set_margins` with a
`drag_end`: claiming a state the client never entered made Phosh believe its
home was already folded, so it never sent `set_state` and the overview could
not fold when an application appeared.

State after this round: lock screen -> Space unlock -> overview (wallpaper,
search, "Show All Apps") -> Ctrl+T opens a floating terminal that can be
dragged by its title bar and closed with the compositor's button. Phosh gets
the toplevel and draws an activity card for it. Next: fold the home reliably
when a toplevel exists, give the top panel a visible bar, and launch from the
app grid.


### A usable desktop: launcher tiles, GIO launching, wallpapers

* **Layer order.** Within a layer, the surface created *first* is on top, as in
  wlroots (new layer surfaces are prepended to the layer list and the list is
  drawn in order). The old "newest on top" fallback put the home's full-screen
  wallpaper above the top panel, so the bar was invisible. Explicit
  stack_below/stack_above still wins; only the fallback changed.
* **Launcher tiles.** Phosh's grid hides apps that do not claim a mobile form
  factor, and its home page only lists favourites. The terminal entry now has
  `X-Purism-FormFactor=Mobile;` and the compositor seeds
  `org.gnome.shell favorite-apps=['werewolf-terminal.desktop']` in the keyfile,
  so a Terminal tile appears on the overview.
* **Launching.** GDesktopAppInfo does not fork the app itself: it execs
  `gio-launch-desktop` (to set GIO_LAUNCHED_DESKTOP_FILE_PID safely). The
  desktop's libexec copy is not in the image - `tools/gio-launch-desktop.c`
  builds a 20-line replacement, installed at /sfs/gio-launch-desktop, and
  `GIO_LAUNCH_DESKTOP` points at it. Clicking the tile now starts /sfs/term.
* **Desktop backdrop.** Phosh's wallpaper lives in the home's background
  surface, which fades out when the overview folds; the background manager
  draws nothing (no configured picture), so the desktop was black. The
  compositor now paints a night-sky vertical gradient (one multiply per row)
  as the root fill.
* **Clicks.** The press/release now route with a motion event so GTK always has
  the position, not just the enter. A first click right after unlocking still
  sometimes only focuses the grid; a second click always activates.

State: lock screen -> Space unlock -> overview (wallpaper, search, Terminal
tile) -> tapping Terminal launches /sfs/term through GIO -> Phosh folds the
overview (the foreign toplevel arrives) -> the terminal floats over the
gradient desktop with a visible top bar, movable by its title bar and closable.


### Where the top panel stands (next session)

The bar is now visible (the layer order fix), but the *panel window itself* is
sitting at margin 0 instead of its folded margin: its 1024x768 buffer has the
clock bar in rows 0..32 and an opaque black quick-settings body below, so the
"desktop" around a window is black and every click on the panel rect would be
claimed as a panel drag (mode FULL). In the folded state the margin must be
-736 (bar rows 736..768 shown at screen 0..32) and the black body must be
off-screen.

Check, in this order:

* the panel's `DragSlot`: `state` (should be 0), `folded` (should be about
  -736, from the client's `set_margins`), `current`;
* `drag_finish()` - a stray press inside the panel rect (which is 0..768 as
  long as the margin is 0) turns into an "unfolded" drag and latches the
  state;
* `layer_record_commit()` copies `slot->margin[]` (the zwlr margins, 0 for
  drag surfaces) over the drag-applied margin; `drag_apply` runs right after,
  so it should win, but the ordering is worth double checking.

Two smaller things seen in the same runs:

* the first click on the app grid right after unlocking sometimes only
  focuses the tile (press and release both reach the client with the same
  target, so it is not a routing bug); a second click always launches;
* the compositor's night-sky gradient root is painted but never visible: the
  panel body (above) covers it. Once the panel folds, it should show.


### Correction: the panel is fine, the black desktop is a background layer

Instrumented the drag slots. At startup and after unlocking:

    MARG ls 64 folded=-736 unfolded=0 current=-736 state=0
    APPLY ls 64 axis=0 margin=-736 height=768

so the top panel *is* folded correctly (bar rows 736..768 shown at screen
0..32) and the earlier note blaming the panel is wrong. The home is unfolded
(`current 0`, state 1) with no toplevel, which is what the overview should be.

With an application window open the home folds and the desktop around it is
*pure black*. The gradient root and a "skip an all-black background layer"
heuristic were both tried; the black pixel survived byte-for-byte, so it is
not the layer-0 `phosh background` surface either (the heuristic samples 16
points of that entry and does not match). Next step is a scene dump of the
folded state (the compositor's in-shmem dump code was removed; add it back
briefly) to see which entry covers (30,300), then fix that surface.


### The black desktop was the quick-settings backdrop (fixed)

A scene dump of the folded state showed five entries; the pixel at (30,300)
was painted by surface 69, `phosh top-panel background`: 1024x768 at layer TOP,
`alpha 255`, `alpha_set 0`, `below 64` (stacked below the folded panel). The
compose-time `layer_effective_alpha` check was already in place, but the
backdrop's pixels had been painted while it was still opaque and later
damage-only repaints never touched that area, so the stale black stayed.

Fix: when a layer surface commits a buffer and it is (a) stacked below a drag
surface, (b) that drag surface is folded and not being dragged, and (c) the
client never set an explicit alpha, the entry is *forgotten* instead of stored
(`layer_forget_surface` marks a full recompose). It comes back on the next
commit, which is exactly what happens when the panel unfolds and Phosh fades
the backdrop in.

Result: with a terminal open the desktop shows the wallpaper (the home's
background surface is back), the top panel bar, and the floating window.


### Unlocking with the mouse

Two bugs together made the lock screen impossible to unlock with the mouse:

* `drag_begin` matched a press against a drag surface by **rectangle only**, and
  Phosh keeps the home (a drag surface) unfolded at full size *underneath* the
  lock screen. Every press anywhere on the screen was claimed as a home drag
  before it could reach the lock screen. A drag surface now only claims the
  press when it is the surface under the pointer (`layer_hit_test`).
* Phosh's unlock is a touch swipe on its deck; a plain mouse click never
  produced it. With `require-unlock=false` any key press unlocks, so a click on
  a *plain* layer surface (no drag slot, not the app window) now sends Space to
  that client and swallows the press/release. On the desktop the topmost
  surface is always a drag surface (panel/home) or the app window, so normal
  clicks are unaffected.

Verified: a plain click on the lock screen unlocks to the overview; the
Terminal tile, launching and the floating window still work.


### Synthetic swipe: tried, does not work

Replaying the press as a swipe (press, eight motions upward 12 ms apart, release,
all with synthetic timestamps) does *not* unlock: libhandy's deck swipe is
touch-only, so the gesture is ignored even at ~5000 px/s. Adding a Space key
after the replay did not help either - the pending pointer sequence seems to
keep GTK from acting on the key until it finishes.

What works and is shipped (`f55e8ba`): a click (or the end of a drag) on the
lock screen sends Space, and `require-unlock=false` turns that into the unlock.
If a real swipe feel is wanted later, the clean way is to make the *lock
screen* respond to pointer drags in the compositor (animate the layer surface
up and finish the unlock), rather than fighting libhandy's touch-only gesture.


### Lock screen swipe (shipped)

The compositor performs the unlock gesture itself, because libhandy's deck
swipe is touch-only. A press on a plain layer surface (no drag slot, not the
app window - i.e. the lock screen) starts `lock_drag`: every motion moves the
surface's `margin[0]` so it follows the pointer up (clamped), and releasing
past 120 px sends the Space key press that unlocks (`require-unlock=false`).
Releasing earlier snaps the surface back.

Verified: dragging the lock screen up shows the overview underneath while the
finger is down, and the release lands on the desktop (screenshot mean jumps
from the lock screen to the overview). Clicking without a drag still unlocks.

A real "finger" feel on top of this would be an ease-out animation of the
margin after the release; the mechanism is in place.


### Launch environment

`tools/gio-launch-desktop.c` now also fills in `WAYLAND_DISPLAY=wayland-test`
and `GDK_BACKEND=wayland` when they are missing. Several launches reached the
helper without `WAYLAND_DISPLAY`, GDK fell back to X11 and the child died with
"cannot open display:", which is the wave of failures in the console after
tapping the launcher tile. Host has one compositor, so the default is always
safe. Needs a guest check: tap the Terminal tile a few times and confirm there
is no "cannot open display:" any more.


### Lock drag animation

Releasing the lock drag no longer snaps: `lock_anim_step()` (called once per
main-loop iteration, alongside input) eases the surface margin from where the
pointer left it to 0 (stay locked) or -OUT_HEIGHT (unlocked, then the Space
key press) over 200 ms with a smoothstep curve. Damage-only composes keep the
cost at one full repaint per animation frame.


### Activity thumbnails: the protocol, ready to implement

`phosh_private.get_thumbnail(id, toplevel, max_width, max_height)` does **not**
return a custom object: the `new_id` is a `zwlr_screencopy_frame_v1`
(`phoc-v0.46.0/protocols/phosh-private.xml`, line 54). Phosh therefore expects
the screencopy frame handshake for a single frame:

1. compositor -> `buffer` (opcode 0): format ARGB8888 (0), width, height,
   stride (width*4). Size = the app window's content scaled to fit
   max_width/max_height, aspect preserved; a 0 means unconstrained.
2. compositor -> `buffer_done` (opcode 6 at version 3+).
3. client -> `copy(frame, buffer)` (request 0).
4. compositor: scale `app_window.copy` (content rect only, skip the CSD
   margins) into the client's wl_shm buffer and send `ready` (opcode 2) with a
   timestamp, then flags if needed.
5. Errors go through `failed` (opcode 3) with the reason enum; `failed` must be
   sent with a valid enum value or libwayland aborts Phosh.

The scaling loop is the only new piece of pixel work (nearest neighbour is
fine at 250x150). Reuse `buffer_find`/`buffer_width` from the pool machinery to
resolve the client's buffer on `copy`, and remember the pending frame per
handle so `copy` knows the size and the source window (the toplevel handle is
the `foreign_handle` created for the app).

Where to hook: the phosh_private branch already ignores opcode 2
(`get_thumbnail`): it takes four u32s. Store `(frame_id, max_w, max_h)` there.


### Thumbnails: the handshake is implemented

`phosh_private.get_thumbnail` is now a real screencopy frame. The offer
computes the thumbnail size from the application window's *content* rectangle
(the CSD margins are skipped), preserving the aspect ratio with integer maths,
and sends `buffer(ARGB8888, width, height, stride)`. On `copy(buffer)` the
compositor resolves the client's shm buffer, scales the stored app frame with
nearest neighbour, then sends `flags(0)`, `ready(timestamp)` and a
`wl_buffer.release`. `failed` is sent for an unknown toplevel, a hidden/absent
window or a buffer whose geometry does not match the offer; `destroy` and
`copy_with_damage` are accepted (`copy_with_damage` answers immediately, a
thumbnail is one still frame).

**Do not send `buffer_done`.** Phosh bundles wlr-screencopy **v2**
(`phosh-v0.57.0/protocol/wlr-screencopy-unstable-v1.xml`,
`zwlr_screencopy_frame_v1 version="2"`), so opcode 6 is an unknown event.
Sending it made libwayland abort Phosh's connection: the shell exited with
`Gdk-Message: Error reading events from display: Resource temporarily
unavailable`. Phoc sends only `buffer` for a thumbnail for the same reason;
the earlier note in this file that asked for `buffer_done` was wrong.

Verified in the guest: launching the Terminal tile creates the activity card,
the compositor logs `phosh: thumb ready 430 290` once, and the overview card
shows the scaled terminal (white body, title bar cropped away by the content
rectangle). Phosh asks again whenever an activity is resized; the card is a
snapshot, not a live mirror, which matches phoc.

### The launcher needs an empty `WAYLAND_DISPLAY` treated as unset

The tile-launch check found `cannot open display:` still. GIO handed
`gio-launch-desktop` a present-but-empty `WAYLAND_DISPLAY`; the helper only
filled the variable in when `getenv()` returned NULL, so GDK fell back to X11
and the child died. Empty is now treated like missing, for `GDK_BACKEND` too.

One caveat remains, unrelated to the environment: a second application started
while the first one is still initialising can lose the race and print the same
message, while one launch always survives. The compositor still keeps
`surface_ids[]` as one global table even though Wayland surface ids are per
client, so the id namespaces are not separated; that is the first suspect and
the next thing to verify. Fixing it means moving the surface table into
`ClientState`, like the buffer table already is.


### New clients no longer wipe Phosh's layer effects

Every accepted connection calls `reset_client()`, and it cleared the
**global** `drag_slots[]` and `alpha_slots[]` tables unconditionally. The
application connecting (the terminal) therefore destroyed Phosh's draggable
home and panel objects. The symptom was confusing: the home folded when the
toplevel appeared, and then a later commit of the home (still animating)
called `layer_record_commit()`, which copies the client's margin (0) into the
layer entry, while `drag_apply()` could no longer find the slot and returned
silently. The overview slid back in behind the application window. The alpha
slots were lost the same way, so later `set_alpha` requests were dropped.

Fix: `DragSlot` and `AlphaSlot` carry the owning client (`current_client` is
now declared before them, because the layer-effect helpers precede
`ClientState`). Lookups match owner **and** id, `layer_effective_alpha()`
matches the entry's owner, and `reset_client()` clears only the slots of the
client being reset. `layer_surface_seq` is no longer zeroed per connection:
it is global stacking order, and resetting it made a new layer surface look
older than Phosh's existing ones.

Verified: launching the Terminal from the tile folds the home and the
desktop stays wallpaper + top bar + floating window (no app grid behind it);
the top panel still unfolds by dragging down *after* the application has
connected. The activity thumbnail still completes.

Known follow-ups, both visible in this flow:

* while an application window is mapped, `poll_input()` sends every event to
  that client, so the shell (home bar, panel) cannot be dragged until the
  window is hidden (Ctrl+T). A real window manager has to hit-test the shell
  layers first and only give the remainder to the app.
* the tile still starts one application per click (the first click focuses,
  the second launches), and a second GTK client starting while the first is
  still initialising can print `cannot open display:`. One window survives.
  `surface_ids[]` is still one global table although surface ids are per
  client; moving it into `ClientState` is the next isolation step.


## 2026-11-14 — Nested phoc (WIP): the real postmarketOS stack

Decision: stop growing the hand-written compositor into a window manager and
run **phoc** (postmarketOS' wlroots compositor) as a nested client instead.
Our compositor becomes a thin host-window shim; phoc owns windows, input,
layer-shell, screenshots and starts Phosh as its shell.

Shipped in this WIP commit:

* `wayland_real.c` has a `SERVE_PHOC` target. It configures the nested output
  at the full 1024x768, draws no compositor chrome/close button, and execs
  `/sfs/phoc -C /sfs/phoc.ini -S -E /sfs/phosh` with
  `WLR_BACKENDS=wayland`, `WLR_RENDERER=pixman`,
  `WLR_RENDERER_ALLOW_SOFTWARE=1`, `WLR_WL_OUTPUTS=1`. `phoc-serve` is built
  with `-DSERVE_ONLY -DSERVE_PHOC`.
* `tools/build_phoc_image.py` installs phoc 0.57.0 (staged in
  `/tmp/alarm/x-phoc`), libwlroots-0.20 and the 17 missing SONAMEs from
  `/tmp/alarm/phocroot` (18 files, 5 MiB), plus a one-line `phoc.ini`
  (`xwayland=false`). The image already had phoc's gschema compiled in.
* Input sources: the `wlroots0.20`, `libinput`, `vulkan-icd-loader`,
  `lcms2`, `seatd`, `libdisplay-info`, `libliftoff`, `xcb-util-*` packages
  were staged with `/tmp/opencode/stage_phoc.py` (pacman closure from the
  aarch64 mirror, cache in `/tmp/alarm/cache`).

Guest result: phoc starts inside our compositor and connects, logs the
expected `drmGetDeviceFromDevId failed` / `Linux dmabuf support unavailable`
(pixman does not need dmabuf), then dies at:

    phoc-server-WARNING: Unable to open wayland socket: Invalid argument

That is `wl_display_add_socket_auto()` failing. Our kernel has no filesystem
socket files at all: AF_UNIX `bind` registers the path in a kernel listener
table (`src/linux_fd.rs`), and `read_sockaddr` clamps to 110 bytes. Suspects
for the EINVAL, in order:

1. libwayland-server's `bind()` sockaddr length (it passes the full
   `sizeof(struct sockaddr_un)`; that should be accepted).
2. a syscall around socket creation (`SOCK_CLOEXEC`, `listen`, a
   `fchmod`/`rename` pair) returning EINVAL and libwayland reporting it.
3. `/tmp` handling for the generated `wayland-N` name.

Next step: instrument `linux_bind`/`linux_socket`/`linux_listen` to print the
errno and the path for the phoc process, or run a tiny guest client that
calls `wl_display_add_socket_auto` against libwayland-server directly.
Once the socket opens, verify Phosh appears through phoc and that input
(including a real XKB keymap, which `phoc` will need) reaches it.


## 2026-11-14 — Nested phoc runs Phosh (the postmarketOS stack)

The real stack is up: `phoc-serve` (this compositor built with `SERVE_PHOC`)
hosts **phoc** nested on the Wayland backend with the pixman renderer; phoc
owns a 1024x768 output and **Phosh runs as its shell**. Verified in the guest:
`Phosh ready`, the lockscreen is rendered through phoc, Space unlocks to the
app grid.

What it needed:

1. **phoc 0.57 + wlroots 0.20 in the image.** `tools/build_phoc_image.py`
   installs phoc, `libwlroots-0.20.so` and the 17 missing SONAMEs (libinput,
   libseat, vulkan loader, lcms2, libdisplay-info, libliftoff, the xcb-util
   set, ...). `phoc.ini` disables Xwayland.
2. **wlroots layer-shell 0-dimension check reverted**, the same downstream
   patch postmarketOS carries (pmaports "temp/wlroots: revert 0 dimension
   error check", MR 1702). Phosh asks for a 0x0 home bar anchored
   bottom|left|right; wlroots 0.20 treats that as a protocol error and kills
   the shell. The builder neutralises the two branches (offsets documented in
   `tools/build_phoc_image.py`).
3. **Kernel gaps this exposed:**
   - `flock(2)` is granted (libwayland's lockfile socket setup needs it);
   - `/dev/shm` with `shm_open`/`shm_unlink` backed by shared memfd state
     (wlroots allocates keymap and swapchain buffers through it);
   - `fchmod`/`fchmodat`/`fchown` succeed, `close_range` works;
   - `statx` is implemented (glib probes it; without it Phosh spun on
     `ENOSYS 291`);
   - `dup`/`dup2` share one VFS descriptor, so `close` now retires the VFS
     fd only when the **last** Linux slot is gone. libxml2 dups
     `xkb/rules/evdev.xml`, closes the original and reads the duplicate;
     without the refcount xkbregistry died with EBADF and phoc had no layouts.
4. **A real XKB keymap from the compositor** (`evdev+us`) instead of
   `NO_KEYMAP`. wlroots builds its own xkb state from the parent keymap; a
   client that only forwards keycodes cannot translate them to keysyms.
   Space now unlocks.
5. **phoc's `-E` session spawn never runs** here: the GLib idle that starts
   the session is not dispatched under the wlroots event loop. The wrapper
   starts phoc with `-S` and then execs Phosh itself on `/tmp/wayland-0` once
   the socket accepts connections — the same split postmarketOS' session
   manager uses.

Known gaps from this first run:

- one pointer click reached phoc with stale surface coordinates (motion/press
  at the old position, release at the new one), so `Show All Apps` did
  nothing. Check the input-ring helper and the app-window local mapping.
- the app grid shows no application tiles (search bar and `Show All Apps` are
  painted), and typed text is not visible in the search entry. Check GIO's
  app enumeration / favourites under Phosh, and the entry's theme.
- pixman performance and a visible host cursor are still open.
