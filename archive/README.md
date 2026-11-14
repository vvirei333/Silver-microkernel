# Research archive

Raw captures kept so old findings can still be bisected and quoted. This is
**not** documentation — the narrative and the reasoning live in
`handoff.md`, and the release-facing status is in `docs/STATUS.md`.

| Path | What it is |
|---|---|
| `cmds.txt` | `sfsinfo` output from the SFS bring-up |
| `qemu_out.txt`, `qemu_err.txt` | serial/host output of early boot runs |
| `demo-tasks-stdout.txt` | stdout of the task/scheduler demo probe |
| `irq-test-stdout.txt` | IRQ/GIC bring-up probe output |
| `logs/` | local stress/stall/QEMU logs from the SMP and Linux ABI work (gitignored, kept for bisecting) |

The tracked files are the captures as originally committed; `logs/` keeps the
raw local runs and is not tracked.
