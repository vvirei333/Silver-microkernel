#!/usr/bin/env python3
"""Run the Linux ABI regression matrix against the tracked disk.img.

The image already contains the test binaries (`tools/linux-test.sh` installs
them). This runner boots the release kernel, waits for `howl>`, then sends each
command and waits for its marker. It is deliberately small: the previous
`/tmp` harness was lost, so the matrix lives in the repo now.

Usage:
    tools/linux_matrix.py
    tools/linux_matrix.py --image /tmp/werewolf-test.img
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
SERIAL = "/tmp/werewolf-matrix.serial"
RELEASE_VERSION = re.search(
    r'^version = "(.+)"$', (ROOT / "Cargo.toml").read_text(), re.MULTILINE
).group(1)

# Installed by tools/linux-test.sh, so they are opt-in: the tracked image
# only carries the cases above.
CAPABILITY_CASES = [
    ("linuxrun /sfs/linux-capmode", b"capmode ok"),
    ("linuxrun /sfs/linux-fexec", b"status=42"),
    ("linuxrun /sfs/linux-fexec-memfd", b"status=42"),
]

CASES = [
    ("linuxrun /sfs/linux-hello", b"hello from linux abi"),
    ("linuxrun /sfs/linux-rw", b"data+bss ok"),
    ("linuxrun /sfs/linux-fork", b"parent saw child 42"),
    ("linuxrun /sfs/linux-cowfork", b"cow fork ok"),
    ("linuxrun /sfs/linux-pipe", b"pipe: ping"),
    ("linuxrun /sfs/linux-ipc", b"ipc ok"),
    ("linuxrun /sfs/linux-signal", b"signal ok"),
    ("linuxrun /sfs/linux-sighandler", b"sigframe ok"),
    ("linuxrun /sfs/linux-asyncsig", b"async sig ok"),
    ("linuxrun /sfs/linux-scope", b"scope ok"),
    ("linuxrun /sfs/linux-threads", b"threads ok"),
    ("linuxrun /sfs/linux-pthread", b"pthread 42"),
    ("linuxrun /sfs/linux-pthread-lifetime", b"worker survived"),
    ("linuxrun /sfs/linux-leader-exit", b"worker survived"),
    ("linuxrun /sfs/linux-kill-group", b"group kill"),
    ("linuxrun /sfs/linux-exitgroup", b"worker exiting"),
    ("linuxrun /sfs/linux-groupsig", b"group signal ok"),
    ("linuxrun /sfs/linux-unixsock", b"unix socket ok"),
    ("linuxrun /sfs/linux-namedsock", b"named socket ok"),
    ("linuxrun /sfs/linux-memfd", b"shared memfd ok"),
    ("linuxrun /sfs/linux-sockets", b"glibc socket ok"),
    ("linuxrun /sfs/linux-eventloop", b"event loop ok"),
    ("linuxrun /sfs/linux-scratch", b"scratch ok"),
    ("linuxrun /sfs/linux-stdio", b"stdio ok"),
    ("linuxrun /sfs/linux-longname", b"long name ok"),
    ("linuxrun /sfs/linux-wayland", b"wayland ok"),
    ("linuxrun /sfs/linux-wlreal", b"libwayland 2 clients ok"),
    ("linuxrun /sfs/linux-wlinfo", b"wayland-info ok"),
    ("linuxrun /sfs/linux-dbus", b"dbus ok"),
    # The version comes from Cargo.toml, so a release bump cannot leave this
    # matrix checking a stale string.
    ("uname", f"v{RELEASE_VERSION}".encode()),
    ("health", b"zombie 0"),
]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", default=str(ROOT / "disk.img"))
    parser.add_argument("--capability", action="store_true",
                        help="also run the capmode/fexec probes from linux-test.sh")
    args = parser.parse_args()
    cases = CASES + (CAPABILITY_CASES if args.capability else [])

    if not KERNEL.exists():
        sys.exit("build first: ./build_and_qemu.sh release --no-run")
    if not Path(args.image).exists():
        sys.exit("missing image: " + args.image)
    try:
        os.unlink(SERIAL)
    except FileNotFoundError:
        pass

    proc = subprocess.Popen([
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "2", "-m", "1024M",
        "-kernel", str(KERNEL),
        "-drive", f"file={args.image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-device", "virtio-net-pci,disable-legacy=on",
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
                cleaned = re.sub(
                    rb"EXIT:[^\r\n]*\r?\n|Program exited with status [0-9]+\r?\n",
                    b"",
                    log,
                )
                if marker in cleaned:
                    return True
            return False

        if not wait(b"howl>", 120):
            print("BOOT FAILED")
            return 1
        failed = []
        for command, marker in cases:
            log.clear()
            sock.sendall((command + "\n").encode())
            ok = wait(marker, 90)
            print(("PASS " if ok else "FAIL ") + command, flush=True)
            if not ok:
                failed.append(command)
                sys.stderr.write(
                    bytes(log)[-2000:].decode("utf-8", "replace") + "\n"
                )
        print(f"\n{len(cases) - len(failed)}/{len(cases)} passed")
        return 1 if failed else 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
