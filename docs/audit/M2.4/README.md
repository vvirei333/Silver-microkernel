# M2.4 audit — IRQ path and Linux dispatcher tails

Fourth (short) hand-off bundle, and the one that closes M2. It covers the two
entry points whose leads were deferred from M2.2: the IRQ path (`R1-R7`) and
the Linux dispatcher/signal/fd tails (`L3-L9`). The snapshot is the current
`main`, i.e. after the M2.3 verdict was applied (backend path hardening, R7
gate aligned with the L1 sanitizer) and after the VFS page-fault known-issue
was documented.

## Documents

| File | What it answers |
|---|---|
| [`irq-entry.md`](irq-entry.md) | IRQ vector → GICv2/GICv3 → bound-driver wake, `bind_irq`/`wait_irq`, unknown-INTID policy, teardown; leads R1-R7 |
| [`linux-dispatch.md`](linux-dispatch.md) | `linux_abi::dispatch`, `rt_sigaction`/`rt_sigreturn`/`siginfo`/altstack, `kill`/`tgkill`, fd table lifetime; leads L3-L9 |

## What changed since M2.2

- **L1 sanitizer** (M2.3): `restore_frame` rejects `pstate & 0x1F != 0` and
  keeps only `NZCV|DAIF`; bit 30 is NZCV.Z, AA32 is M[4].
- **R7 gate aligned**: `silver_irq_signal_return` now checks `frame[32] & 0x1F`
  instead of `& 0xF`, so an AArch32 frame (impossible today, but the same gate
  as L1) can never receive an AArch64 signal frame.
- **V-dot hardening** (M2.3): the SFS backend delegates path splitting to
  `sfs_format::split_path_name`, rejecting `.`/`..`/empty/over-long
  components even if the VFS normaliser is bypassed. Unit test added.
- **Known gap #11**: lazy file-backed page faults read SFS under the BKL — a
  latency known-issue, not a boundary break (M3 performance item).

Not changed: the IRQ and signal/fd code itself. R1-R7/L3-L9 are the review of
that code as shipped.

## Snapshot manifest

```
07492b0c1ac628951c13fb557446c996fde87153c47d4822ea3e8fa84d5d200f  gic.rs
37a936a58f0c81b2c6e549cf7ff0a5a18e80e7cb7100a159c3350b93b766db9b  gicv3.rs
066c76c8a602511653de86d8c1d5d09251d141a35b6cd5f43a05685e3f73a797  irq_policy.rs
f40e0bf7d36987a011f7c607ec516dcc9919fcb1794f26ab72bef0909de10b69  linux-dispatch.rs
618380666ead4b398f495ab88dccab1a095f3ab11ae7535eeafdb52dffe9ed0e  linux_elf.rs
fcba1ede470ed57eead1b7637f2feb2f8cb960199d22bee38f48e86de0bcac16  linux_fd.rs
5fbfe86507e6bbc0352c17c6b3fb8ff37cfaa979f40bbb0e532e12e631bcb49e  linux_signal.rs
454cdc81df1eb7a55c540dba10b3cf63596089f1d22ef85a9c109c73fa4ba8bd  main-irq.rs
121c08d98ecb4325c83e8df01d2e71e040c2ab97e5d5b6ac94542de55b71efe3  scheduler-irq.rs
3fc82c17aac97f346803b8090cbaaea9f7cfa7beb69ccf592e1d3a1243afaecd  smp-irq.rs
9d0f8bbac8f850c16e037830cecf5df41dd45d8e4de8a85eb2aac76ba2b48994  syscall-irq-hook.rs
17e18427b4b4220f144bb936cdfbe68233027655a86e79b4a5eb8d4c0a239c81  timer.rs
78c7ae74ea31fde203ab15756eb078b71f23748c8dbf61a15e2c35662ea4efa7  wait_queue.rs
```

Whole files: `gic.rs`, `gicv3.rs`, `irq_policy.rs`, `wait_queue.rs`,
`timer.rs`, `linux_signal.rs`, `linux_fd.rs`, `linux_elf.rs`. Excerpts
(line-numbered in the header): `main-irq.rs` (vectors + IRQ trampoline +
signal hook), `scheduler-irq.rs` (wait/bind/exit/kill/reap), `smp-irq.rs`,
`syscall-irq-hook.rs`, `linux-dispatch.rs` (dispatcher head, altstack,
kill/tgkill).

## M2 closure checklist

Accepted in M2.3 and recorded in [`../M2.3/README.md`](../M2.3/README.md):
fault/COW path clean with the VFS-under-BKL performance note; ELF parser
clean; SFS/VFS clean with the backend path hardening now applied.

Open at the time of writing (owner/auditor to close with this packet):

1. R1-R7 and L3-L9 — this snapshot; verdicts close M2.
2. Native probes still listed in `tests/security/README.md`:
   `mailbox_quota`, `dma_cross_read`, `grant_foreign`, `blocked_kill`,
   `irq_orphan`, `linux_fd_lifetime`. (`fd_lifetime` covers the kernel
   SCM_RIGHTS class today.)
3. M3 hand-off: per-region `CAP_DMA_SHARE` + revocation, page-fault I/O
   outside the BKL, FGT trap for EL0 `daifset` on platforms that have it.

The fuzzer and `--no-linux` build are already CI gates; M3 keeps them.
