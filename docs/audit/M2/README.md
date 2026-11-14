# M2 audit — entry points

This directory is the hand-off bundle for the M2 security pass. It contains a
frozen snapshot of the two entry points the auditor walks first, an annotated
path through each one, and the checklist of invariants to test against the
CVE map. The snapshot is code, not a description: every claim below can be
checked against `snapshot/` without the working tree.

- **Audited commit:** `1b0f52d` (M1.5). `src/` is identical to the 0.1.2
  release commit `dc1dcd7`; only docs and research logs changed after it.
- **Scope of this pass:** SVC (EL0 → kernel) and mailbox IPC (send/receive).
  IRQ delivery, task faults, the Linux syscall bodies and the filesystem are
  separate entry points and are only referenced where control is handed to
  them.

## Documents

| File | What it answers |
|---|---|
| [`syscall-entry.md`](syscall-entry.md) | How `svc #0` reaches a handler: vector → frame → BKL → dispatch → epilogue; what is trusted; invariants S1-S8; leads Q1-Q5 |
| [`ipc-entry.md`](ipc-entry.md) | Mailbox send/receive, identity, queue limits, death cleanup, nameserver RPC; invariants I1-I10 |

## Snapshot manifest

SHA256 of the files in `snapshot/`:

```
2ebb56f38b295b1b00ada939923bdc3e57b884202b9d060899eda2989e3a76de  capability.rs
3659bbd6838c80acb107fad328b880860a36903056d57dd71f7c5f0549138ee7  condvar.rs
f324af93a05f865217db11a9933782f146f09529bf8e5fb5634a7c715355888d  ipc.rs
ad0bb55da37afb713bf0c90931c0dbf6d507af370d2b2d25f82d92475b50d7cd  linux_abi.rs
3fe40d4cecf58f3aad763526c24a8d319b4add741b9b470c57f79a27b8943c32  main-vectors.rs
b77a1221d97a36fa658e90c8746f595c33e683b254e0715ab5ec0731ef361e89  mmu-user-access.rs
ab75f01d34ce633da0c290130f20f9a98ac72b832c31921f51eaf1fed235e719  scheduler-ipc.rs
b7adc613e7d0e9475a380195518732c03e3393775086b7d659ab19b9f69cdf26  syscall.rs
eba60f322d3fc89e61c0d875b76e8c0aff46168e05a76be32f0e9445106f4274  user_memory.rs
78c7ae74ea31fde203ab15756eb078b71f23748c8dbf61a15e2c35662ea4efa7  wait_queue.rs
```

`main-vectors.rs` is the whole `src/main.rs` (the assembly trampoline lives
there); `mmu-user-access.rs` and `scheduler-ipc.rs` are line-numbered excerpts
of the two large modules, marked as such at the top of each file.

## How to use it

1. Walk `syscall-entry.md` with the snapshot open. Every numbered invariant is
   a testable claim; the leads (Q1-Q5) are places where the code's own
   comments make an assumption that deserves a hostile test.
2. Walk `ipc-entry.md` the same way. I1-I3 and I8 are policy questions as much
   as memory-safety ones.
3. Report findings as `file:line` + a reproducer sketch. Guest-side
   regression tests land in `tests/security/`; the runner is
   `tools/security-test.sh`. The first case (fd lifetime/ownership across
   IPC, from the M0.2 post-mortem) is already there as the template.

## Suggested order after these two

- IRQ → kernel (`exception_irq_entry`, GIC/timer, driver `wait_irq`) and task
  faults (`exception_user_fault`, COW path) — the other two ways into the
  kernel.
- Linux personality bodies (`linux_abi.rs`): 32-bit argument truncation,
  `ERESTARTSYS`, signal-frame validation, fd table lifetime.
- The filesystem write path (`CAP_FS_WRITE`, path scopes) and SFS parsing of
  on-disk structures.
