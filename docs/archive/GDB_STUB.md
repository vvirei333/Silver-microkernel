# GDB remote stub (second PL011)

`src/gdb.rs` speaks the GDB remote serial protocol on the **second** QEMU
PL011 (`0x0904_0000`), so console output never interleaves with debugger
traffic. It is feature-gated (`gdb-stub`) and runs from the user-fault path:
when a task dies, the kernel prints its register bank, then serves the
debugger until it detaches.

Build and boot:

```bash
cargo build --release --target aarch64-unknown-none \
    --no-default-features --features no_std,linux-abi,gdb-stub
qemu-system-aarch64 ... \
    -serial unix:/tmp/console.serial,server=on,wait=off \
    -serial unix:/tmp/werewolf-gdb.serial,server=on,wait=off
```

`tools/gdb_rsp.py` is a small, deterministic client (gdb itself sends whole
packets that overflow QEMU's 16-byte PL011 RX FIFO; a byte-at-a-time relay
helps but is not required for this client):

```bash
tools/gdb_rsp.py                 # registers
tools/gdb_rsp.py x19:0x100       # memory at x19
tools/gdb_rsp.py --walk          # frame-pointer chain
```

## Supported packets

| Packet | Meaning |
|---|---|
| `?` | stop reason (`S05`) |
| `g` / `G` | read registers (x0..x30, sp, pc, cpsr) |
| `m addr,len` | read the faulting task's memory |
| `M addr,len:hex` | write the faulting task's memory |
| `c` / `s` / `D` | release the task to the normal kill path |
| `qSupported`, `qAttached` | minimal feature negotiation |
| `qXfer:features:read:target.xml` | aarch64 register description |

Memory access uses the faulting task's active TTBR0. A request for an
unmapped user address would fault in EL1, which is fatal: stick to addresses
taken from registers (objects, stack) when inspecting.

## Hardware write watchpoints

`src/debug.rs` arms DBGWVR0/DBGWCR0 (LSC = store) and enables monitor debug in
MDSCR_EL1. The native shell exposes it:

```
howl> watch 0x50000000      # 8-byte write watch (length optional)
howl> unwatch
```

`tests/linux/watchtest.c` maps a fixed page and stores into it, so
`watch 0x50000000` followed by `linuxrun /sfs/watchtest` is the self-test.
On hardware the store raises EC 0x34 and the kernel enters the stub.

**QEMU `virt` does not deliver the hardware exception.** The registers read
back with the written values, but the store completes (with `-smp 1` too, so
it is not task migration). The same `watch` command also arms a **software
page watch**, which QEMU honours: mappings that cover the watched page clear
their write bit (`lazy_page_fault`, eager `mmap_region`), and a write fault
whose FAR is inside the watched range stops in the stub. A write to another
field of the same page is allowed through (the page-level watch is
one-shot), which is enough for the self-test and for a first look at a
corrupted object.

Verified with `watch 0x50000000` + `linuxrun /sfs/watchtest`: the stub stops
with `pc` at the store and `x0 = 0x50000000`.

## Limits

- No single-step or breakpoints yet; the fault is final.
- `gdb` expects `vMustReplyEmpty` and other modern packets; the stub answers
  empty, which is enough for `info registers`/`x` when the transport keeps up.
  The host-side FIFO overrun (QEMU's PL011 has a 16-byte RX FIFO and drops
  bytes from a whole-packet write) is the remaining obstacle for interactive
  gdb, hence `tools/gdb_rsp.py`.
