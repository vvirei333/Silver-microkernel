#!/usr/bin/env python3
"""Talk to the WerewolfOS GDB stub (second PL011).

The kernel must be built with the `gdb-stub` feature and QEMU started with a
second serial socket, e.g.

    cargo build --release --target aarch64-none         --no-default-features --features no_std,linux-abi,gdb-stub
    qemu-system-aarch64 ... \
        -serial unix:/tmp/console.serial,server=on,wait=off \
        -serial unix:/tmp/werewolf-gdb.serial,server=on,wait=off

Run this script *before* the guest faults: the stub only speaks after a user
fault, and bytes written to an unconnected chardev are dropped.

    tools/gdb_rsp.py                       # registers only
    tools/gdb_rsp.py x19:0x100             # memory at x19
    tools/gdb_rsp.py x19+0x90:0x10         # a field
    tools/gdb_rsp.py --walk                # frame-pointer chain
"""
import socket
import sys
import time


class Rsp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        for _ in range(200):
            try:
                self.sock.connect(path)
                break
            except OSError:
                time.sleep(0.1)
        self.buffer = b""

    def _fill(self):
        chunk = self.sock.recv(4096)
        if not chunk:
            raise SystemExit("connection closed")
        self.buffer += chunk

    def wait_banner(self, marker=b"GDB-STUB aarch64"):
        while marker not in self.buffer:
            self._fill()
        self.buffer = b""

    def send(self, payload: bytes) -> bytes:
        self.sock.sendall(b"$" + payload + b"#" + f"{sum(payload) & 0xFF:02x}".encode())
        while True:
            while b"$" not in self.buffer:
                self._fill()
            start = self.buffer.index(b"$") + 1
            while b"#" not in self.buffer[start:]:
                self._fill()
            end = self.buffer.index(b"#", start)
            reply = self.buffer[start:end]
            self.buffer = self.buffer[end + 3:]
            # Ack the stub's reply and drop any stray '+' from earlier packets.
            self.sock.sendall(b"+")
            self.buffer = self.buffer.lstrip(b"+")
            return reply

    def memory(self, address, length):
        reply = self.send(f"m{address:x},{length:x}".encode())
        if reply.startswith(b"E"):
            return None
        return bytes.fromhex(reply.decode())


def main():
    rsp = Rsp("/tmp/werewolf-gdb.serial")
    rsp.wait_banner()
    print("stop:", rsp.send(b"?").decode())
    text = rsp.send(b"g").decode()
    names = [f"x{i}" for i in range(31)] + ["sp", "pc", "cpsr"]
    values = {}
    position = 0
    for name in names:
        size = 8 if name == "cpsr" else 16
        chunk = text[position:position + size]
        position += size
        values[name] = int(chunk, 16)
        print(f"{name:>5}=0x{values[name]:016x}")

    if "--walk" in sys.argv:
        fp = values["x29"]
        for _ in range(12):
            data = rsp.memory(fp, 16)
            if not data or len(data) < 16:
                break
            saved_fp = int.from_bytes(data[0:8], "little")
            saved_lr = int.from_bytes(data[8:16], "little")
            print(f"frame fp=0x{fp:016x} lr=0x{saved_lr:016x}")
            if saved_fp <= fp or saved_fp == 0:
                break
            fp = saved_fp
    x19 = values["x19"]
    for spec in sys.argv[1:]:
        base, length = spec.split(":")
        if base.startswith("x"):
            name, _, extra = base.partition("+")
            base = values[name] + (int(extra, 0) if extra else 0)
        else:
            base = int(base, 0)
        data = rsp.memory(base, int(length, 0))
        print(f"mem[0x{base:x}+{length}] = {data.hex() if data else None}")
        if data:
            for offset in range(0, len(data), 8):
                value = int.from_bytes(data[offset:offset + 8], "little")
                print(f"  +{offset:04x}: {value:016x}")

    rsp.send(b"D")
    print("detached")


if __name__ == "__main__":
    main()
