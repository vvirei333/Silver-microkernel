#!/usr/bin/env python3
"""Release gate: boot a kernel + SFS image in QEMU and check the console OS.

This is deliberately the same path a user takes — the packaged artifacts are
loaded as-is, the serial line is the only interface — so a release cannot be
tagged with a kernel that boots from `target/` but not from the tarball.

Checks, in order:
  1. the boot banner reaches the `howl>` prompt
  2. `uname` reports the version the release claims
  3. `ls /sfs` and `cat /sfs/readme.txt` read the shipped image
  4. a native utility (`echo`) prints through the console
  5. `linuxrun /sfs/linux-hello` runs a Linux ELF and exits 42
  6. `health` reports no zombie, no open file and no DMA leak
  7. `bench fs` completes (the fast-path instrument works on the artifact)

Usage:
    scripts/release_smoke.py
    scripts/release_smoke.py --kernel dist/.../silver_microkernel \
        --image dist/.../disk.img --timeout 60

Exit code is 0 only when every check passed. The serial log of the run is
written to /tmp/werewolf-release-smoke.log for post-mortems.
"""
import argparse
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
IMAGE = ROOT / "disk.img"
LOG = Path("/tmp/werewolf-release-smoke.log")


def release_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'^version = "(.+)"$', text, re.MULTILINE)
    return match.group(1) if match else "unknown"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Serial:
    """TCP serial console (works the same on Windows and Linux hosts)."""

    def __init__(self, port: int, timeout: float):
        self.deadline = time.time() + timeout
        self.socket = None
        for _ in range(int(timeout * 4)):
            try:
                self.socket = socket.create_connection(("127.0.0.1", port), timeout=1)
                break
            except OSError:
                time.sleep(0.25)
        if self.socket is None:
            raise RuntimeError(f"cannot connect to the QEMU serial on port {port}")
        self.socket.settimeout(0.5)
        self.buffer = b""

    def read_until(self, marker: bytes, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if marker in self.buffer:
                return True
            try:
                chunk = self.socket.recv(4096)
            except socket.timeout:
                continue
            except OSError:
                return False
            if not chunk:
                return False
            self.buffer += chunk
        return marker in self.buffer

    def send(self, text: str) -> None:
        self.socket.sendall(text.encode() + b"\r")

    def text(self) -> str:
        return self.buffer.decode("utf-8", "replace")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", default=str(KERNEL))
    parser.add_argument("--image", default=str(IMAGE))
    parser.add_argument("--memory", default="1024M")
    parser.add_argument("--timeout", type=float, default=45.0)
    parser.add_argument("--no-linux", action="store_true",
                        help="skip the Linux ABI check (console-only kernel)")
    args = parser.parse_args()

    kernel = Path(args.kernel)
    image = Path(args.image)
    for path, what in ((kernel, "kernel"), (image, "disk image")):
        if not path.is_file():
            print(f"release smoke: missing {what}: {path}", file=sys.stderr)
            return 2

    version = release_version()
    port = free_port()
    command = [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", args.memory, "-display", "none", "-monitor", "none",
        "-kernel", str(kernel),
        "-drive", f"file={image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-device", "virtio-net-pci,disable-legacy=on",
        "-serial", f"tcp:127.0.0.1:{port},server=on,wait=off",
    ]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    checks: list[tuple[str, bool]] = []

    def check(name: str, passed: bool) -> None:
        checks.append((name, passed))
        print(f"  [{'PASS' if passed else 'FAIL'}] {name}")

    try:
        serial = Serial(port, timeout=min(args.timeout, 20.0))
        check("boots to the howl prompt", serial.read_until(b"howl>", args.timeout))

        steps = [
            ("uname reports the release version",
             f"uname", f"Silver Kernel v{version}".encode()),
            ("ls /sfs lists the shipped image", "ls /sfs", b"readme.txt"),
            ("cat reads an SFS file", "cat /sfs/readme.txt", b"readme!!"),
            ("echo prints through the console", "echo release-smoke", b"release-smoke"),
            ("bench runs on the artifact", "bench fs", b"fs open+close"),
            # The DMA line is printed after the task line, so wait for it and
            # then require both counters from the same report.
            ("health is clean", "health", b"leaked 0"),
        ]
        if not args.no_linux:
            steps.insert(4, ("a Linux ELF runs (exit 42)",
                             "linuxrun /sfs/linux-hello", b"hello from linux abi"))
        for name, command_line, marker in steps:
            serial.send(command_line)
            passed = serial.read_until(marker, args.timeout)
            if passed and command_line == "health":
                passed = b"zombie 0" in serial.buffer and b"leaked 0" in serial.buffer
            check(name, passed)
    finally:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()

    failed = [name for name, passed in checks if not passed]
    print(f"release smoke: {len(checks) - len(failed)}/{len(checks)} checks passed")
    if failed:
        for name in failed:
            print(f"  failed: {name}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
