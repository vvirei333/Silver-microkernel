# M2.3 audit — EL0 faults/COW and VFS/SFS

Third hand-off bundle. The snapshot is the current `main` (after the M2.2
verdict was applied: PSTATE sanitizer, F1 quota, F2 share capability, F3
recipient gate).

## Documents

| File | What it answers |
|---|---|
| [`fault-path.md`](fault-path.md) | EL0 sync fault → `silver_handle_user_fault` → COW repair or task kill; fork/COW producers; leads C1-C8 |
| [`vfs-sfs.md`](vfs-sfs.md) | VFS path layer and the SFS on-disk parser (superblock, inodes, indirect blocks, dirents, bitmap); leads V1-V8 |

The disk image and installed bundles are treated as untrusted input, same
class as the network: a corrupt image must be rejected, not panic or free
kernel-owned blocks.

## What changed after the M2.2 verdict

- **L1 sanitizer.** `restore_frame` rejects `pstate & 0x1F != 0` (EL1/EL0
  modes and the AArch32 selector in M[4]) and keeps only NZCV|DAIF
  (`0xF000_0000 | 0x3C0`); SS/IL/BTYPE/PAN/reserved are stripped.
  **Correction to the review table:** in SPSR_EL1 bit 30 is NZCV.**Z**, not
  AA32 — the AArch32 selector is M[4] (bit 4), already covered by the 0x1F
  check. Terminating on bit 30 would kill any handler returning with Z set.
  `tests/security/sigreturn_flags.c` proves Z survives and SS/IL are stripped;
  `sigreturn_aa32.c` proves M[4]=1 is rejected.
- **F1** per-(sender,target) quota: `MAX_MESSAGES_PER_SENDER = 64`; a blocking
  `send` returns `QueueFull` instead of pinning the mailbox. `try_send`
  unchanged in spirit (drops).
- **F3** `grant_capability` only delegates to the caller itself or a task it
  spawned (`recipient.ppid == caller`). `demos.rs`'s concurrent-wait grant is
  parent→own-child, so the probe still exercises delegation.
- **F2** `CAP_DMA_SHARE` is a target-scoped right naming the DMA owner; the
  driver and the network clients receive it from silverd (and the shell
  delegates it to Wolfser through `SYS_CAP_FIND`). `CAP_MMIO` alone no longer
  maps another task's window. Per-region scoping is the M3 refinement.

## Snapshot manifest

```
f3ebeb5f51cd14d2e4de71d22a54ebf4a961fa6dc4343dcce43563d552c82c8e  elf.rs
c23afa40b5859a28f88c80e80534ecd02693432980e53b7af7d3bb46288e9c88  exceptions.rs
09b0c98bab1b1ca03daf5a1ce1e14b116446a1544a7c2dcf11d496678cda0cc6  main-fault.rs
992bf3e8ee844315255c73e3e3ddef3dd2f29df0ae2718adf86c9030d4ceefea  mmu-fault.rs
e9da90f6820a9cd50bf24af284824174756f158359e9c2f341705f93e91dd0f9  scheduler-fault.rs
12cc9c9b0ce7a061079a47e2fee1833382ab469c69909e296c24ab45e5f9f36e  sfs_format.rs
a461174ee54618f5b0ceca309008a427276e1b42678446e0e35cd83e54809645  sfs.rs
93e7d71c7ff3eb97cf0d50d9f6516497ff65d05945bb225d36581777823ac8f5  vfs.rs
```

`exceptions.rs`, `vfs.rs`, `sfs.rs`, `sfs_format.rs` and `elf.rs` are whole
files; `main-fault.rs`, `mmu-fault.rs` and `scheduler-fault.rs` are
line-numbered excerpts (fault vector, remap/COW helpers, fork/COW bodies).
