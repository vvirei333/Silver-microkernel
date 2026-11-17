#!/usr/bin/env python3
"""Prove `apk` runs inside the guest from the Alpine-only image.

Injects tools/apk_info_launcher.c (compiled static) as /apk-info, boots the
image, and runs it at the `howl>` prompt. Success means apk started, found
its package database and listed installed packages without network access.

    tools/build_alpine_phosh.py
    tools/alpine_apk_smoke.py
"""
import argparse
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from sfs_inject import Image  # noqa: E402

KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
SERIAL = "/tmp/werewolf-apk.serial"
LAUNCHER = ROOT / "build/apk-info"


def build_launcher() -> None:
    LAUNCHER.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        os.environ.get("LINUX_CC", "aarch64-linux-gnu-gcc"),
        "-static", "-O2", "-Wl,-z,max-page-size=4096",
        "-o", str(LAUNCHER), str(ROOT / "tools/apk_info_launcher.c"),
    ], check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", default="/tmp/werewolf-alpine-phosh.img")
    args = parser.parse_args()
    image = Path(args.image)
    if not KERNEL.exists():
        sys.exit("build first: ./build_and_qemu.sh release --no-run")
    if not image.exists():
        sys.exit("build the image first: tools/build_alpine_phosh.py")
    build_launcher()

    img = Image(str(image))
    img.write_file_at("/apk-info", LAUNCHER.read_bytes())
    img.save()

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
        log.clear()
        sock.sendall(b"linuxrun /sfs/apk-info\n")
        if not wait(b"alpine-baselayout", 120):
            sys.stderr.write(bytes(log)[-1500:].decode("utf-8", "replace"))
            print("FAIL apk info")
            return 1
        print("PASS apk info (Alpine package database)")
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
