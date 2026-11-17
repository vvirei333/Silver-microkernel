#!/usr/bin/env python3
"""Boot the Alpine phosh image in QEMU and drive it over a TCP serial.

Windows Python has no AF_UNIX, so the PL011 serial is exposed on TCP
127.0.0.1:4444 instead of a unix socket. The script boots the kernel, waits
for the `howl>` prompt, sends the given commands and prints everything the
guest wrote until the final marker or a timeout.

Usage:
    tools/qemu_serial.py --kernel target/.../silver_microkernel \
        --image build/werewolf-alpine-phosh.img \
        --send "linuxrun /sfs/sbin/init" --wait "WAYLAND SOCKET OK" --timeout 300
"""
import argparse
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

KERNEL = (ROOT / "target/aarch64-unknown-none/release/silver_microkernel")
import os
PORT = int(os.environ.get("WEREWOLF_SERIAL_PORT", "4444"))


def boot(image: Path, kernel: Path, memory: str, extra: list[str]) -> subprocess.Popen:
    command = [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", memory,
        "-kernel", str(kernel),
        "-drive", f"file={image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-display", "none", "-monitor", "none",
        "-serial", f"tcp:127.0.0.1:{PORT},server=on,wait=off",
    ] + extra
    return subprocess.Popen(command, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)


class Serial:
    def __init__(self, timeout: float = 30.0) -> None:
        deadline = time.time() + timeout
        last: OSError | None = None
        while time.time() < deadline:
            try:
                self.sock = socket.create_connection(("127.0.0.1", PORT), timeout=2)
                break
            except OSError as error:
                last = error
                time.sleep(0.2)
        else:
            raise SystemExit(f"cannot connect to serial: {last}")
        self.sock.settimeout(0.3)
        self.log = bytearray()

    def pump(self) -> None:
        try:
            self.log.extend(self.sock.recv(65536))
        except socket.timeout:
            pass

    def wait(self, marker: bytes, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump()
            if marker in self.log:
                return True
        return False

    def send(self, line: str) -> None:
        self.sock.sendall(line.encode() + b"\n")

    def tail(self, count: int = 3000) -> str:
        return self.log[-count:].decode("utf-8", "replace")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, default=KERNEL)
    parser.add_argument("--memory", default="1024M")
    parser.add_argument("--send", action="append", default=[],
                        help="commands to send after the howl> prompt")
    parser.add_argument("--wait", action="append", default=[],
                        help="markers awaited in order after each send")
    parser.add_argument("--timeout", type=float, default=120.0,
                        help="per-marker timeout in seconds")
    parser.add_argument("--extra", action="append", default=[],
                        help="extra QEMU arguments (verbatim)")
    parser.add_argument("--once", action="store_true",
                        help="exit after the last marker instead of streaming")
    args = parser.parse_args()

    proc = boot(args.image, args.kernel, args.memory, args.extra)
    try:
        serial = Serial()
        if not serial.wait(b"howl>", 120):
            print("BOOT FAILED", flush=True)
            print(serial.tail())
            return 1
        print("howl> up", flush=True)
        for command, marker in zip(args.send, args.wait):
            serial.log.clear()
            serial.send(command)
            if marker:
                if not serial.wait(marker.encode(), args.timeout):
                    print(f"MARKER MISSING: {marker}", flush=True)
                    print(serial.tail())
                    return 1
                print(f"MARKER HIT: {marker}", flush=True)
        # keep streaming until Ctrl+C for interactive debugging sessions
        while not args.once:
            serial.pump()
            time.sleep(0.2)
        serial.pump()
        print(serial.tail(4000))
    except KeyboardInterrupt:
        pass
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    return 0


if __name__ == "__main__":
    sys.exit(main())
