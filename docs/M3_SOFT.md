# M3-soft — status and evidence

Work order: four security probes, per-region `CAP_DMA_SHARE`, page-fault I/O
outside the BKL. The first five items are landed and verified; the page-fault
item is designed but **not** landed (see the last section).

## Status

| # | Item | State | Evidence |
|---|---|---|---|
| 1.1 | `mailbox_quota` (F1) | **done** | native probe; 64 fill + 10 blocking sends rejected; victim drains and answers |
| 1.2 | `blocked_kill` (L2) | **done** | mask readback strips bits 8/18, SIGKILL reaps the child; post-suite health 0/0/0 |
| 1.4 | `linux_fd_lifetime` (M0.2) | **done** | 1000 SCM_RIGHTS rounds with number churn |
| 2 | per-region `CAP_DMA_SHARE` | **done** | `SYS_DMA_GRANT_SELF`, `dma_region_confine`, net server owner grants |
| 1.3 | `irq_orphan` (R3) | **done** | GIC `ISENABLER` read before/after kill; rebind succeeds |
| 3 | page-fault I/O outside the BKL | **designed, not landed** | design below |

Security suite: **12/12** cases (11 M3-soft + `mmio_range_confine` from
M3.1), a post-suite `health` with zombie 0 / open 0 / leaked 0, and 200k fuzz
cases. Matrix 26/26, Phosh demo clean, `--no-linux` clean, 224 host unit
tests.

## Negative controls

Every new gate was verified failing with the gate reverted, then passing on
the shipped tree:

| Gate reverted | Probe result |
|---|---|
| `MAX_MESSAGES_PER_SENDER = 1024` | `FAIL mailbox_quota` |
| `UNBLOCKABLE = 0` | `FAIL blocked_kill` (mask readback contains SIGKILL) |
| `shared_dma = region_owner.is_some()` | `FAIL dma_cross_read`, `FAIL dma_region_confine` |
| `release_bound_irq` clears without masking | `FAIL irq_orphan` |

`linux_fd_lifetime` is a stronger version of the M0.2 regression whose
failing-then-passing proof already exists in `fd_lifetime` (the kernel
SCM_RIGHTS lifetime fix). It does not change kernel code.

## Per-region DMA sharing

Before this slice, `CAP_DMA_SHARE` named an owner task, so any holder could map
every DMA run that task owned. Now:

- `SYS_DMA_GRANT_SELF(recipient, base, size)` is the **owner's** consent. The
  kernel checks `[base, base + size)` against the caller's own `dma_pages`
  and inserts a capability that names both the owner and the range.
- `map_phys` accepts a foreign run only when `has_dma_share(owner, base, size)`
  finds a region-scoped capability covering the whole request.
- `SYS_CAP_GRANT` rejects `CAP_DMA_SHARE`: a generic handle covers a task, not
  a range, and an owner-wide delegation was the F2 hole. Spawn handles do not
  authorize a foreign map.
- `net_server` grants its main mailbox range to the driver when it attaches,
  and each per-client 1 MiB window to that client when the window is created.
  silverd/shell no longer hand out owner-wide share rights; the shell's ping
  and Wolfser work through the server's consent.
- A grant is not revocable yet (SECURITY.md gap #2).

## Page-fault I/O outside the BKL — design

Current path: EL0 data abort → `silver_handle_user_fault` takes `BklGuard` →
`cow_fault` → `lazy_page_fault` → `vfs::read_path_at` → SFS cache →
`virtio_blk::read_sector` polled. The whole chain runs under the BKL, so on a
uniprocessor every task waits for the page.

Why the obvious shortcut is not safe: releasing the BKL around the VFS read
while holding `&mut Task` and the VFS mutex breaks the lock order (kernel
mutexes assume the BKL) and allows another thread to `munmap`/`mprotect` the
region underneath the copy. The read also has to revalidate the mapping
after re-acquiring.

Proposed async path:

1. `lazy_page_fault` detects a file-backed non-resident page, records
   `(task id, ttbr0/ASID, path, file offset, destination frame)` in a bounded
   page-in queue, marks the task `Blocked(waiting-page)`, and returns a retry
   code. The fault handler releases the BKL and schedules; an interrupt
   cannot resume the task until the request completes.
2. A kernel page-in worker (or the virtio-blk completion IRQ, once the block
   driver is IRQ-driven instead of polled) performs the read. It must be able
   to touch the VFS/SFS cache without the BKL, which means a dedicated
   cache/device lock and a lock order documented next to the BKL one.
3. On completion the worker maps the page into the owner's `ttbr0` (or hands
   the frame back for the retry to map), performs the TLB invalidation for
   that ASID, then wakes the task with the page marked resident. The retry
   re-executes the faulting instruction; a second fault for the same page is
   a no-op (the residency check under the BKL wins).
4. Teardown: a dying owner's queued requests are cancelled and their frames
   released; a stale request cannot map into a recycled ASID (generation tag
   on the task address space, same idea as the COW run generation).

Acceptance measurement (not yet run): on `-smp 1`, task A repeatedly touches
file-backed `mmap` pages while task B increments a counter / prints ticks.
Today B's ticks freeze for the duration of each page read; with the async
path B must advance during the block I/O. The matrix, the 11 security cases,
the Phosh demo and `--no-linux` remain gates.

This is the next implementation slice; the rest of M3-soft is closed.
