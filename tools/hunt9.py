#!/usr/bin/env python3
"""hunt9: catch the phoc 0x11 corruption with a QEMU gdbstub ladder.

A Z1 breakpoint at phoc+0x46ACC stops inside `phoc_xdg_toplevel_constructed`
right before the `wl_signal_add` for `request_resize` — the last call before
the load at 46aec/46af0 that faults with `self->xdg_toplevel == 0x11`. Each
stop yields x19 (the toplevel `self`); an 8-byte Z2 write watch is armed on
`self+144`. Any later store to that address stops the VM with full registers;
foreign tasks that happen to use the same VA are stepped over silently until
a stop with PC in the phoc image range (0x10000000..0x14000000) — the
corrupting store.

    WEREWOLF_SERIAL_PORT=4465 python tools/hunt9.py
"""
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

from qemu_serial import Serial  # noqa: E402
from qemu_gdb_watch import GdbClient, parse_registers  # noqa: E402

KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
IMAGE = ROOT / "build/werewolf-alpine-phosh.img"
BREAK = 0x10046ACC
PHOC_MIN, PHOC_MAX = 0x10000000, 0x14000000
KERNEL_MIN, KERNEL_MAX = 0x40000000, 0x50000000


def main() -> int:
    command = [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", "1024M",
        "-kernel", str(KERNEL),
        "-drive", f"file={IMAGE},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-display", "none", "-monitor", "none",
        "-serial", "tcp:127.0.0.1:4465,server=on,wait=off",
        "-gdb", "tcp::1234",
    ]
    proc = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    stops = open(ROOT / "build/hunt9_stops.txt", "w")
    try:
        serial = Serial(timeout=60)
        if not serial.wait(b"howl>", 120):
            print("BOOT FAILED"); print(serial.tail(1000)); return 1
        print("howl> up", flush=True)
        gdb = GdbClient(1234)
        gdb.command("qSupported")
        # Setting breakpoints requires a halted VM: interrupt, arm, resume.
        gdb.sock.sendall(b"\x03")
        print("interrupt:", gdb.read_packet(20), flush=True)
        print("Z1:", gdb.command(f"Z1,{BREAK:x},4"), flush=True)
        gdb.send("c")
        serial.send("run /sfs/armwatch")
        serial.wait(b"armwatch", 30)
        serial.log.clear()
        serial.send("linuxrun /sfs/sbin/init")
        print("ladder armed; watching ...", flush=True)

        watched: set[int] = set()
        for attempt in range(20000):
            serial.pump()
            stop = gdb.read_packet(20)
            if stop is None:
                if b"far=0x00000000000000e9" in serial.log:
                    print(f"CRASH (far=e9) without a phoc-range stop after "
                          f"{attempt} events; watched={len(watched)}")
                    print(serial.tail(2000))
                    return 2
                continue
            regs = parse_registers(gdb.command("g") or "")
            pc = regs.get("pc", 0)
            stops.write(f"{attempt} stop={stop} pc={pc:#x} x19={regs.get('x19', 0):#x}\n")
            stops.flush()
            if pc == BREAK:
                x19 = regs.get("x19", 0)
                field = x19 + 144
                print(f"toplevel stop: self={x19:#x}", flush=True)
                if field not in watched and x19 != 0:
                    watched.add(field)
                    print("  Z2:", gdb.command(f"Z2,{field:x},8"), flush=True)
                gdb.send("c")
                continue
            if KERNEL_MIN <= pc < KERNEL_MAX:
                gdb.send("s"); gdb.read_packet(20)
                gdb.send("c")
                continue
            # a real watch hit from an EL0 task
            gdb.send("s"); gdb.read_packet(20)
            stops.write(f"pc={pc:#x} x0={regs.get('x0', 0):#x} "
                        f"x1={regs.get('x1', 0):#x} x19={regs.get('x19', 0):#x}\n")
            stops.flush()
            if PHOC_MIN <= pc < PHOC_MAX:
                print(f">>> WRITER IS PHOC: pc={pc:#x}", flush=True)
                for name in ("x0", "x1", "x2", "x19", "x20", "sp", "x30"):
                    if name in regs:
                        print(f"  {name} = {regs[name]:#018x}")
                return 0
            gdb.send("c")
        print("stop budget exhausted")
        return 1
    finally:
        stops.close()
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
