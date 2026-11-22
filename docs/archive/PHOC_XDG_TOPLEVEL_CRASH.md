# phoc `0x11` crash: which field, which instruction

Status: M3 blocker, investigation notes (2026-11-17). No fix yet; this file
records what the fault is *not* and narrows where the bad value appears.

## Image and versions

`/tmp/werewolf-alpine-phosh.img` (Alpine-only, built by
`tools/build_alpine_phosh.py`).

- `phoc` **0.55.1-r0** (Alpine community), not 0.57.0 as earlier notes said.
- wlroots is vendored inside the phoc build: the debug line table and DWARF
  paths name `subprojects/wlroots-0.20.x/types/...`.
- `phoc-dbg` from the same package supplies the DWARF used below
  (`/tmp/phoc-dbg/usr/lib/debug/usr/bin/phoc.debug`); the executable itself
  (`/tmp/alpine-phosh/usr/bin/phoc`) is stripped and is what `objdump` reads.

## Fault

```
FAULT task=8 esr=0x0000000092000006 far=0x00000000000000e9 elr=0x0000000010046af0
x0=0x0000000000000011  x19=0x0000000044A41E40  x30=0x0000000020051C44
SP_EL0=0x000000007FE3E440 ...
```

This is a **single-task** report. The earlier dumps in this investigation were
assembled from many small console writes and two CPUs faulting at the same
time could interleave their registers into one report (that is how a dump with
`x19=0` next to this ELR appeared). The kernel now formats the whole report,
including the task id, FAR, ELR and ESR, in one buffer and publishes it with
one console write; `tools/fault_dump_check.py` is the regression.

`ESR 0x92000006` is a data abort, read (`WnR=0`), translation fault level 2.
The PIE base is `0x1000_0000`, so ELR is `phoc+0x46AF0`.

## Instruction

`objdump -d /tmp/alpine-phosh/usr/bin/phoc` around `0x46AE8`:

```
46aec:  f9404a60    ldr x0, [x19, #144]   ; self->xdg_toplevel
46af0:  f9406c00    ldr x0, [x0,  #216]   ; ->events.request_maximize
```

- DWARF `ptype /o PhocXdgToplevel`: `xdg_toplevel` is at **offset 144**.
- `0x11 + 216 = 0xE9`, exactly FAR, so `self->xdg_toplevel == 0x11` at the
  load.
- `gdb info line *0x46af0` resolves the instruction to
  `src/xdg-toplevel.c:666` (phoc v0.55.1):

```c
self->request_maximize.notify = handle_request_maximize;
wl_signal_add (&self->xdg_toplevel->events.request_maximize,
               &self->request_maximize);
```

## What the window proves

`phoc_xdg_toplevel_constructed` dereferences the same field several times
before line 666, and all of those succeed:

| line | access |
|------|--------|
| 653  | `wl_signal_add (&self->xdg_toplevel->events.destroy, ...)` |
| 656  | `wl_signal_add (&self->xdg_toplevel->base->events.new_popup, ...)` |
| 660  | `wl_signal_add (&self->xdg_toplevel->events.request_move, ...)` |
| 663  | `wl_signal_add (&self->xdg_toplevel->events.request_resize, ...)` |
| 666  | `wl_signal_add (&self->xdg_toplevel->events.request_maximize, ...)` — fault |

Between the `request_resize` registration (663) and the `request_maximize`
registration (666) the function only stores the callback pointer at
`self+328` and loads `self+144`. No phoc code assigns to offset 144 there.

So the sequence is:

1. `phoc_xdg_toplevel_set_property` (`xdg-toplevel.c:124`) stored a valid
   `wlr_xdg_toplevel *` and dereferenced it (`self->xdg_toplevel->base->data =
   self`), so the pointer was good when the property was set.
2. `g_assert (self->xdg_toplevel)` at the top of `constructed` passes.
3. The first four `wl_signal_add` calls use the pointer successfully.
4. By line 666 the field is `0x11`.

The value is overwritten **inside `constructed`**, after
`G_OBJECT_CLASS (phoc_xdg_toplevel_parent_class)->constructed (object)`
returns, and after at least the `request_resize` list insertion. `0x11` (17)
looks like a small integer / Wayland object id, not a pointer.

This rules out:

- the property system passing a bad value (set_property would have faulted at
  `[0x11+8] = 0x19` inside its own body);
- a missing `g_assert` (the value is non-NULL, so the assert cannot catch it);
- the earlier “stale anonymous heap page” hypothesis: `mmu::map_user_heap_pages`
  already calls `zero_page()` for every new program-break page, so `brk`
  growth does not expose recycled frames.

## Why the crash is timing-sensitive

The field is valid at the first four listener insertions and bad at the fifth,
so whatever writes offset 144 is not phoc source code at that point. Two
families remain:

1. a wild store from adjacent code (e.g. a `wl_list` insertion whose list head
   is corrupt, or a struct that is written at the same address);
2. a register/stack restore anomaly between the two calls (the kernel saves
   and restores x19 across EL0 calls, so this would have to come from a signal
   frame or a corrupted GOT/PLT resolution).

Debug logging changes the allocation/timing pattern and sometimes survives
past output creation, which fits a corruption that depends on object placement
rather than a deterministic error return.

## How to catch the writer next

The object page is **stable across runs**: observed `self` values are
`0x44A41E40` and `0x44A41EA0`, both inside `0x44A41000`. That makes
page-granular tooling usable.

1. **QEMU gdbstub hardware watchpoint** (best): boot the image with `-s`,
   `hbreak *0x10047704` (`phoc_xdg_toplevel_set_property`), read `x19` (self),
   then `watch *(unsigned long *)($x19+144)` and `continue`. QEMU TCG does
   deliver its own gdbstub watchpoints, unlike the guest DBGWCR path. The
   earlier attempt did not reach phoc because the full `init` chain takes
   several minutes; run phoc directly from the `howl>` prompt after
   `openrc sysinit`, or precompile `gschemas.compiled` into the image.
2. **Software page watch with logging**: the kernel's `watch <addr>` protects
   the containing page and stops in the GDB stub on the first write. It is
   one-shot today, so extend it with an “allow and re-protect” mode that logs
   `FAR` and `ELR` for every write to the page. The writer of offset 144 will
   then appear as one of the logged PCs.
3. **Canary in phoc**: patch `phoc_xdg_toplevel_set_property` to also store a
   sentinel in a spare slot, or assert the pointer before every
   `wl_signal_add`. This needs a phoc rebuild against the same vendored
   wlroots, which is heavier than 1 and 2.

## Update: the same field from phosh

`tools/build_alpine_phosh.py` runs `phoc -S -E /usr/bin/phosh`; phosh creates
its GTK windows as xdg-toplevels. If a build without a client (`phoc -S`, no
`-E`) never reaches `phoc_xdg_toplevel_constructed`, the corruption belongs to
the client-driven path rather than compositor startup. This is worth checking
before instrumenting further.
