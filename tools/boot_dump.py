#!/usr/bin/env python3
"""Boot the denial image, wait for a failure marker, pmemsave via monitor.

    python -I tools/boot_dump.py --wait "fatal error" --dump build/fbst.bin
"""
import argparse
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# qemu_serial reads the port at import time - set it before the import.
os.environ.setdefault("WEREWOLF_SERIAL_PORT", "4446")
sys.path.insert(0, str(ROOT / "tools"))

from qemu_serial import Serial  # noqa: E402

KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
IMAGE = ROOT / "build/werewolf-alpine-glibc-denial.img"
SERIAL_PORT = int(os.environ["WEREWOLF_SERIAL_PORT"])
MONITOR_PORT = 4445


def qemu_command() -> list[str]:
    return [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", "1024M",
        "-kernel", str(KERNEL),
        "-drive", f"file={IMAGE},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-device", "virtio-gpu-pci",
        "-display", "none",
        "-monitor", f"tcp:127.0.0.1:{MONITOR_PORT},server=on,wait=off",
        "-serial", f"tcp:127.0.0.1:{SERIAL_PORT},server=on,wait=off",
    ]


def monitor_command(command: str, timeout: float = 10.0) -> str:
    sock = socket.create_connection(("127.0.0.1", MONITOR_PORT), timeout=timeout)
    sock.settimeout(timeout)
    try:
        time.sleep(0.3)
        try:
            sock.recv(65536)  # banner / prompt
        except socket.timeout:
            pass
        sock.sendall((command + "\n").encode())
        time.sleep(0.3)
        chunks = b""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                data = sock.recv(65536)
            except socket.timeout:
                break
            if not data:
                break
            chunks += data
            if b"(qemu)" in data:
                break
        return chunks.decode(errors="replace")
    finally:
        sock.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wait", default="fatal error")
    parser.add_argument("--dump", default="build/fbst.bin")
    parser.add_argument("--timeout", type=float, default=170.0)
    parser.add_argument("--log", default="build/denial-boot-dump.log")
    args = parser.parse_args()

    env = dict(os.environ)
    env["WEREWOLF_SERIAL_PORT"] = str(SERIAL_PORT)
    proc = subprocess.Popen(qemu_command(), stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    log = open(ROOT / args.log, "wb")
    dumped = False
    try:
        serial = Serial(timeout=60)
        if not serial.wait(b"howl>", 120):
            print("BOOT FAILED"); print(serial.tail(1000)); return 1
        print("howl> up", flush=True)
        serial.send("linuxrun /sfs/sbin/init")
        # "log end" lands after the stderr replay cat, so the whole deniald
        # output is in the serial log by dump time.
        if not serial.wait(b"DENIAL: log end", args.timeout):
            print("no deniald exit within timeout, dumping anyway", flush=True)
        else:
            print("deniald exited, dumping rings", flush=True)
        log.write(bytes(serial.log))
        log.flush()
        monitor_command("stop")
        monitor_command(
            f"pmemsave 0x40000000 0x800000 \"{(ROOT / args.dump).as_posix()}\"")
        dumped = True
        print(f"dumped to {args.dump}", flush=True)
        monitor_command("quit")
    finally:
        log.close()
        if not dumped:
            proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    return 0


if __name__ == "__main__":
    sys.exit(main())
