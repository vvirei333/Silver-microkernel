# M2.2 audit — Linux dispatcher, signal frames, IRQ path

Second hand-off bundle for the M2 pass. The snapshot is the current `main`
(after the M2.1 fixes), so the two findings below can be verified against the
code that is being shipped, not against a stale tree.

## Documents

| File | What it answers |
|---|---|
| [`linux-dispatch.md`](linux-dispatch.md) | `linux_abi::dispatch`, argument/errno handling, the signal-frame machinery, **finding L1 (critical, fixed)** and L2 (minor, fixed), leads L3-L9 |
| [`irq-entry.md`](irq-entry.md) | IRQ vector → GICv2/v3 → bound-driver wake, `bind_irq`/`wait_irq`, unknown-INTID policy, teardown; leads R1-R7 |

## Findings already fixed in this packet

**L1 — critical: `rt_sigreturn` let a task choose its exception level.**
`restore_frame` copied the user's `pstate` into `frame[32]`; the trampoline
wrote it to `SPSR_EL1` and `eret`ed. `PSTATE.M = EL1h` (0x5) returned to EL1
at the user's PC with the user's x0-x30. Proof on the unfixed kernel:

```
EL1 EXCEPTION kind=0
ESR_EL1=000000008600000F
ELR_EL1=0000000000400114
KERNEL PANIC: halting
```

With a kernel gadget as PC (no KASLR) and controlled registers this is an
arbitrary EL1 call — local privilege escalation from any Linux task. Fixed by
rejecting `pstate & 0xF != 0` (Linux `valid_user_regs` policy); the task is
killed with SIGSEGV. Regression: `tests/security/sigreturn_pstate.c`, which
fails with the panic above on the pre-fix tree and prints `sigreturn pstate
blocked` on this one.

**L2 — minor: SIGKILL/SIGSTOP could be blocked,** making a task unkillable
(pending bit never delivered). `set_blocked` now strips both bits.

Also fixed from the M2.1 report and covered by tests/docs elsewhere:

- **F4** — the kernel-side nameserver RPC is bounded
  (`NAME_RPC_TIMEOUT_TICKS = 3000`); a hung Scout returns `SYSCALL_TIMEOUT`.
- **F6** — one shared-console write is capped at 256 KiB.
- **HTTP chunk DoS** — remote spin fixed, fuzz regression
  (`tests/security/fuzz/`).

## Snapshot manifest

```
0bf68ed55617e91c9fc08fde152bef02e164a0224fea81a2f0c89337f7f03095  gic-irq.rs
48bba866f815342316a995f98a508c749aa6a3d4bca9502156d183a19d88b375  gicv3-irq.rs
066c76c8a602511653de86d8c1d5d09251d141a35b6cd5f43a05685e3f73a797  irq_policy.rs
ad0bb55da37afb713bf0c90931c0dbf6d507af370d2b2d25f82d92475b50d7cd  linux_abi.rs
fcba1ede470ed57eead1b7637f2feb2f8cb960199d22bee38f48e86de0bcac16  linux_fd.rs
d824b97fc4e0a1a7447600166ba4150a68e56fdacabe4b4594977c8a85036d5f  linux_signal.rs
cc8952eaf0ced3c6aced785e7b00f2b29161cec9cd5827d452b6f59e5007511a  main-irq.rs
c3ec626ab6afc333cd9ac0d77e2a824c6d36a42984778d757946606ee899a822  scheduler-irq.rs
```

`linux_abi.rs`, `linux_fd.rs`, `linux_signal.rs` and `irq_policy.rs` are
whole files; the other three are line-numbered excerpts (`main-irq.rs` = IRQ
trampoline, `gic-irq.rs` / `gicv3-irq.rs` = the two controllers,
`scheduler-irq.rs` = wait/bind/exit).

## Verification on this snapshot

- Linux ABI matrix: 26/26 (including signals, pthreads, exit_group,
  unix sockets, Wayland, D-Bus, health 0/0/0).
- `cargo test --lib`: 222 passed.
- `tools/security_test.py --fuzz`: `fd_lifetime` and `sigreturn_pstate`
  pass; HTTP fuzzer 200000 cases, no panic.
