#!/usr/bin/env python3
"""Catch the phoc corruption with QEMU's own gdbstub write watchpoint.

QEMU TCG honours gdbstub watchpoints (unlike the guest DBGWCR path), so a
Z2 (write watch) packet on the exact field address stops the machine on the
corrupting store with full registers. The field is stable across runs:
PhocXdgToplevel self = 0x44A41E40, xdg_toplevel at offset 144.

Usage:
    tools/qemu_gdb_watch.py --image build/werewolf-alpine-phosh.img \
        [--watch 0x44A41ED0] [--serial-port 4455] [--gdb-port 1234]

The tool boots the kernel, waits for `howl>`, starts the Alpine session and
watches the guest over QEMU's gdbstub. On a watchpoint stop it prints the
full register bank plus the PC/LR disassembly context markers.
"""
import argparse
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"


class GdbClient:
    def __init__(self, port: int) -> None:
        deadline = time.time() + 30
        while True:
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=2)
                break
            except OSError:
                if time.time() > deadline:
                    raise SystemExit("gdbstub not reachable")
                time.sleep(0.2)
        self.sock.settimeout(0.5)
        self.buffer = bytearray()
        self.ack()

    def send(self, command: str) -> None:
        packet = f"${command}#"
        checksum = 0
        for byte in command.encode():
            checksum = (checksum + byte) & 0xFF
        packet += f"{checksum:02x}"
        self.sock.sendall(packet.encode())

    def ack(self) -> None:
        self.sock.sendall(b"+")

    def read_packet(self, timeout: float = 30.0) -> str | None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            # scan for a complete $...#xx
            start = self.buffer.find(b"$")
            if start >= 0:
                end = self.buffer.find(b"#", start)
                if end >= 0 and len(self.buffer) >= end + 3:
                    payload = self.buffer[start + 1 : end].decode("utf-8", "replace")
                    del self.buffer[: end + 3]
                    self.ack()
                    return payload
            try:
                self.buffer.extend(self.sock.recv(4096))
            except socket.timeout:
                pass
        return None

    def command(self, command: str, timeout: float = 30.0) -> str | None:
        self.send(command)
        return self.read_packet(timeout)

    def continue_until_stop(self, timeout: float) -> str | None:
        self.send("c")
        return self.read_packet(timeout)


def parse_registers(reply: str) -> dict[str, int]:
    # QEMU aarch64 g-packet: x0..x30, sp, pc, cpsr (8-byte LE each) followed
    # by floating point data we do not need.
    names = [f"x{i}" for i in range(31)] + ["sp", "pc", "cpsr"]
    values: dict[str, int] = {}
    body = reply.split("#")[0] if "#" in reply else reply
    if body.startswith("OK") or not body:
        return values
    offset = 0
    for name in names:
        chunk = body[offset : offset + 16]
        if len(chunk) < 16:
            break
        values[name] = int.from_bytes(bytes.fromhex(chunk), "little")
        offset += 16
    return values


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, default=KERNEL)
    parser.add_argument("--watch", default="0x44A41ED0",
                        help="field address to watch for writes")
    parser.add_argument("--serial-port", type=int, default=4455)
    parser.add_argument("--gdb-port", type=int, default=1234)
    parser.add_argument("--memory", default="1024M")
    parser.add_argument("--timeout", type=float, default=600.0,
                        help="seconds to wait for the watchpoint hit")
    args = parser.parse_args()

    address = int(args.watch, 16)
    sys.path.insert(0, str(ROOT / "tools"))
    from qemu_serial import Serial  # reuse the serial driver

    command = [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", args.memory,
        "-kernel", str(args.kernel),
        "-drive", f"file={args.image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-display", "none", "-monitor", "none",
        "-serial", f"tcp:127.0.0.1:{args.serial_port},server=on,wait=off",
        "-gdb", f"tcp:::{args.gdb_port}" if False else f"tcp::{args.gdb_port}",
    ]
    proc = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    try:
        serial = Serial(timeout=30)
        if not serial.wait(b"howl>", 120):
            print("BOOT FAILED")
            print(serial.tail(2000))
            return 1
        print("howl> up")

        gdb = GdbClient(args.gdb_port)
        print("gdbstub:", gdb.command("qSupported:multiprocess+;xmlRegisters=aarch64"))
        result = gdb.command(f"Z2,{address:x},8")
        print(f"Z2 watch {address:#x}:", result)
        if result != "OK":
            print("watchpoint rejected")
            return 1

        serial.send("run /sfs/armwatch")
        serial.wait(b"armwatch", 20)
        serial.log.clear()
        serial.send("linuxrun /sfs/sbin/init")

        stop = gdb.continue_until_stop(args.timeout)
        if stop is None:
            print("NO STOP within timeout; serial tail:")
            print(serial.tail(3000))
            return 1
        print("STOP REPLY:", stop)
        registers = parse_registers(gdb.command("g") or "")
        for name in ("x0", "x1", "x2", "x19", "x20", "x22", "sp", "pc", "x30"):
            if name in registers:
                print(f"  {name} = {registers[name]:#018x}")
        # one continue so the guest keeps running for further inspection
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
