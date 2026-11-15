# Security tests

Three kinds of checks live here:

1. **Guest regressions** (`*.c`) — freestanding Linux ABI programs that drive
   a kernel boundary and print a marker. They run on the real kernel in QEMU,
   because the behaviour under test (fd tables, SCM_RIGHTS, capabilities)
   only exists there.
2. **Native guest regressions** (`userspace/src/bin/secprobe.rs`, embedded in
   the kernel and run with `secprobe <case>`) — EL0 programs that need the
   Silver capability model (`SYS_CAP_GRANT`, `SYS_MAP_PHYS`), which the Linux
   ABI does not expose. The shell grants `CAP_MMIO` for the run; the probe
   itself has no initial rights.
3. **Host fuzz targets** (`fuzz/*.rs`) — deterministic harnesses for parsers
   that consume untrusted bytes. The parser module is shared with the guest
   (`userspace/src/http_parse.rs`), so a host crash is a guest crash.

## Run

```bash
# build the release kernel first
./build_and_qemu.sh release --no-run

tools/security_test.py                 # compile + inject + boot + run all cases
tools/security_test.py --fuzz          # ... and run the host HTTP parser fuzzer
tools/security_test.py --case fd_lifetime
tools/security_test.py --no-boot       # compile and inject only
```

The runner never touches the tracked `disk.img`: it copies it to
`/tmp/werewolf-security.img` and injects the compiled programs there. The
kernel is `target/aarch64-unknown-none/release/silver_microkernel`; the QEMU
command mirrors `build_and_qemu.sh` (no display, serial on a Unix socket).

## Cases

| Case | Proves |
|---|---|
| `fd_lifetime` | An fd passed over `SCM_RIGHTS` is **duplicated/refcounted**, not remembered by number: the sender closes its copy, the freed number is recycled by unrelated memfds, and the receiver still reads the original file. Regression for the M0.2 class (fd lifetime / ownership across IPC) moved from user space into the kernel boundary. |
| `sigreturn_pstate` | `rt_sigreturn` must reject a ucontext whose `pstate` is not EL0t. The child sets `PSTATE.M = EL1h` in its handler and traps with `udf` right after the interrupted syscall: on the unfixed kernel this is an `EL1 EXCEPTION` panic (and, with a kernel PC, an arbitrary EL1 call); with the fix the child dies with SIGSEGV and the parent prints the marker. |
| `sigreturn_aa32` | `PSTATE.M[4] = 1` (AArch32) is a different execution state; the frame must be rejected (SIGSEGV), not `eret`ed into an unsupported mode. |
| `sigreturn_flags` | The sanitizer keeps NZCV (bit 30 is `Z`, not the AArch32 selector) and strips SS/IL/reserved: the handler sets `Z|SS|IL|bit23`, the task must resume at EL0 and print the marker. |
| `dma_cross_read` | **Native.** A task with `CAP_MMIO` but no capability naming a DMA owner cannot map that owner's window; after the orchestrator delegates the target-scoped `CAP_DMA_SHARE` (the silverd path) the map succeeds and the owner observes the byte written through it. |
| `grant_foreign` | **Native.** `SYS_CAP_GRANT` refuses a live non-child pid (the shell), while the same handle still delegates to the caller itself and to its own child. |

Planned (from the M2 audit reports in `docs/audit/M2/` and `docs/audit/M2.2/`):

- `send_spam` — a task filling another task's mailbox (ambient send-by-pid).
- `munmap_toctou` — a sibling thread unmapping a receive buffer after the
  pre-block validation; the copy-out must revalidate.
- `name_squat` — duplicate/owner-replacement policy for `R`/`L` names.
- `linux_fd_lifetime` — the Linux personality's fd table under
  `close_range`/`dup`/`fork` (the M0.2 class on the ABI side).
- `sigreturn_pc` — the follow-up to `sigreturn_pstate`: an EL0t frame with a
  kernel PC must die on an EL0 instruction abort, never execute at EL1.
- `blocked_kill` — a task that blocks SIGKILL must still die.
- `mailbox_quota` — a native sender cannot exceed 64 unread messages per
  target (needs a native probe; the Linux ABI has no `SYS_SEND`).
- `irq_orphan` — bind an SPI and exit before draining; the next interrupt
  must be masked as unknown, not halt the machine (R1).

## Host fuzz targets

`fuzz/http_parse_fuzz.rs` compiles `userspace/src/http_parse.rs` directly with
`rustc` and feeds it mutated responses: content-length edge values, chunked
bodies, missing CRLF, oversized sizes, `received` larger than the buffer.
Panics are findings; the harness prints the input as hex.

It found the first real bug on its first run: a chunk size of
`0xFFFFFFFFFFFFFFFF` made `input = input.saturating_add(size)` reach
`usize::MAX`, the following `input + 1` wrapped to zero in release builds and
`dechunk` spun forever — a remote DoS on any chunked HTTP response. The fix
clamps `input` to the buffer length; the oversized-chunk seed is now the
harness's named regression (it runs before the random phase, so a regression
hangs the harness instead of slipping through).

Tune the iteration count with `HTTP_FUZZ_ITERS` (default 200000, ~30 ms).
The harness is deterministic: a failing input is reproducible from the
printed iteration/hex.

## Adding a case

1. Drop `name.c` here, freestanding (`-nostdlib`, raw `svc` wrappers — copy
   the header from `fd_lifetime.c`), with a single `_start`.
2. Print a unique marker (`"<name> ok\n"`) and exit 0 on success; print the
   failure reason and exit 1 otherwise.
3. Add `"name": b"<name> ok"` to `CASES` in `tools/security_test.py`.
4. If it needs a shared module, add a host harness under `fuzz/` instead and
   wire it into `run_fuzz()`.
