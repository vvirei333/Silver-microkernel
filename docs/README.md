# WerewolfOS documentation

Everything in this directory describes the Silver microkernel and the
configurations built on it. Source and behaviour are the truth; a document that
disagrees with a boot log is a bug in the document.

## Start here

| If you want to… | Read |
|---|---|
| install and run it | [INSTALL.md](INSTALL.md) — QEMU quickstart, checksums, board status |
| see what is actually done | [STATUS.md](STATUS.md) — capabilities, limits, test matrix, measured numbers |
| understand the shape of the system | [ARCHITECTURE.md](ARCHITECTURE.md) — layers, boot chain, repository layout |
| work on the kernel | [KERNEL.md](KERNEL.md) — memory, scheduler, IPC, drivers |
| work on the console userspace | [USERSPACE.md](USERSPACE.md) — silverd, Howl, net_server, toolbox |
| work on the Linux ABI | [LINUX_ABI.md](LINUX_ABI.md) — what runs, what is missing |
| work on the filesystem | [FILESYSTEMS.md](FILESYSTEMS.md) — SFS, RamFs, synthetic `/proc`/`/sys`/`/dev` |
| work on drivers | [DRIVER_MODEL.md](DRIVER_MODEL.md) — driver/device model and its deviations |
| understand a decision | [DESIGN_DECISIONS.md](DESIGN_DECISIONS.md) — decisions with reasons, including superseded ones |
| see the plan | [ROADMAP.md](ROADMAP.md) — phases measured against the real code |
| know where the code is going | [REFACTOR.md](REFACTOR.md) — modularisation plan and its rules |
| work on the desktop track | [DENIAL.md](DENIAL.md), [VECTOR.md](VECTOR.md) |
| read the boot/IPC/capability rules | [SECURITY.md](SECURITY.md), [CONFIGURATIONS.md](CONFIGURATIONS.md) |
| contribute | [CONTRIBUTING.md](CONTRIBUTING.md) |

`../handoff.md` is a chronological engineering log, not a specification: it is
where session notes accumulate, including dead ends. Read it for history, not
for current behaviour.

`archive/` holds superseded documents (old plans, per-milestone audits, the
dated Russian architecture snapshot). They describe deleted code and are kept
for provenance only.

## Status legend

Documents and the status table use four markers:

| Marker | Meaning |
|---|---|
| **IMPLEMENTED** | proven by code plus a boot self-test or a reproducible QEMU run in this repository |
| **PARTIALLY IMPLEMENTED** | the path exists and runs, with a named gap |
| **EXPERIMENTAL** | enabled behind a flag, not part of a product configuration |
| **PLANNED** | design only; no claim of working behaviour |

## Reproducing what the documents claim

Every number in [STATUS.md](STATUS.md) comes from a command in the repo:

| Claim | Command |
|---|---|
| release boots and behaves | `scripts/release_smoke.py` (8 checks) |
| fast paths | `bench` at the `howl>` prompt |
| host-side unit tests | `cargo test --lib` (229 tests) |
| SMP scheduler probes | build with `no_std,linux-abi,smp-selftest`, boot `-smp 4` |
| Linux ABI matrix | `tools/linux_matrix.py` |
| capability/IPC security suite | `secprobe <case>` at the shell |
| filesystem structure | `scripts/sfs_fsck.py disk.img` |
| release bundle | `scripts/make_release.sh --smoke` |
