#!/usr/bin/env python3
"""Smoke test the Alpine userland image: BusyBox (musl) runs under Linux ABI.

Build the image first with tools/build_alpine_env.py.
"""
import argparse
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
SERIAL = "/tmp/werewolf-alpine.serial"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", default="/tmp/werewolf-alpine.img")
    args = parser.parse_args()
    image = Path(args.image)
    if not KERNEL.exists():
        sys.exit("build first: ./build_and_qemu.sh release --no-run")
    if not image.exists():
        sys.exit("build the image first: tools/build_alpine_env.py")

    try:
        os.unlink(SERIAL)
    except FileNotFoundError:
        pass
    proc = subprocess.Popen([
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", "1024M",
        "-kernel", str(KERNEL),
        "-drive", f"file={image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-display", "none", "-serial", f"unix:{SERIAL},server=on,wait=off",
        "-monitor", "none",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        sock = socket.socket(socket.AF_UNIX)
        for _ in range(300):
            try:
                sock.connect(SERIAL)
                break
            except OSError:
                time.sleep(0.1)
        sock.settimeout(0.2)
        log = bytearray()

        def wait(marker, timeout):
            end = time.time() + timeout
            while time.time() < end:
                try:
                    log.extend(sock.recv(65536))
                except socket.timeout:
                    pass
                if marker in log:
                    return True
            return False

        if not wait(b"howl>", 120):
            print("BOOT FAILED")
            return 1
        sock.sendall(b"linuxrun /sfs/busybox\n")
        if not wait(b"BusyBox v", 30):
            print("FAIL alpine busybox")
            sys.stderr.write(bytes(log)[-2000:].decode("utf-8", "replace") + "\n")
            return 1
        print("PASS alpine busybox")
        log.clear()
        sock.sendall(b"linuxrun /sfs/apk\n")
        if wait(b"apk-tools 3", 30):
            print("PASS alpine apk-tools")
            return 0
        print("FAIL alpine apk-tools")
        sys.stderr.write(bytes(log)[-2000:].decode("utf-8", "replace") + "\n")
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
