#!/usr/bin/env python3
"""Drive a freestanding Linux probe (tests/linux/input.rs by default) in QEMU.

Boots the kernel with a virtio keyboard and tablet and runs the command. For
the evdev probe it injects key presses through the HMP monitor and
ABS_X/ABS_Y through QMP. With --command/--expect it runs another probe (for
example the sysfs one) and only checks the expected success line. It also
runs `health` and `svc` afterwards so a session shows the kernel stays clean.

The binary must already be in the image as `/sfs/linux-input`; run
`tools/linux-test.sh` first (or inject with `scripts/sfs_inject.py`).

Usage:
    python tools/input_probe.py
    python tools/input_probe.py --image build/werewolf-test.img --timeout 60
"""
import argparse
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"


def connect(port: int, timeout: float = 40.0) -> socket.socket:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=2)
            sock.settimeout(0.3)
            return sock
        except OSError as error:
            last = error
            time.sleep(0.2)
    raise SystemExit(f"cannot connect to port {port}: {last}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, default=ROOT / "disk.img")
    parser.add_argument("--kernel", type=Path, default=DEFAULT_KERNEL)
    parser.add_argument("--memory", default="1024M")
    parser.add_argument("--serial-port", type=int, default=4560)
    parser.add_argument("--monitor-port", type=int, default=4561)
    parser.add_argument("--qmp-port", type=int, default=4563)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--marker", default="devices=2",
                        help="log marker to wait for before injecting keys")
    parser.add_argument("--keys", action="store_true",
                        help="with a custom probe, send key a when it reports added=2")
    parser.add_argument("--command", default="linuxrun /sfs/linux-input")
    parser.add_argument("--expect", default="input ok")
    args = parser.parse_args()

    command = [
        "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
        "-smp", "4", "-m", args.memory,
        "-kernel", str(args.kernel),
        "-drive", f"file={args.image},format=raw,if=none,id=hd",
        "-device", "virtio-blk-pci,drive=hd",
        "-device", "virtio-keyboard-pci",
        "-device", "virtio-tablet-pci",
        "-display", "none",
        "-monitor", f"tcp:127.0.0.1:{args.monitor_port},server=on,wait=off",
        "-qmp", f"tcp:127.0.0.1:{args.qmp_port},server=on,wait=off",
        "-serial", f"tcp:127.0.0.1:{args.serial_port},server=on,wait=off",
        "-snapshot",
    ]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    log = bytearray()

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            try:
                log.extend(serial.recv(65536))
            except socket.timeout:
                pass

    try:
        serial = connect(args.serial_port)
        deadline = time.time() + 120
        while time.time() < deadline and b"howl>" not in log:
            pump(0.5)
        if b"howl>" not in log:
            print("BOOT FAILED")
            print(log.decode("utf-8", "replace")[-4000:])
            return 1

        log.clear()
        serial.sendall(args.command.encode() + b"\n")
        if args.command.strip() != "linuxrun /sfs/linux-input":
            if args.keys:
                deadline = time.time() + 20
                while time.time() < deadline and args.marker.encode() not in log:
                    pump(0.5)
                monitor = connect(args.monitor_port)
                time.sleep(2.0)
                for key in (b"a", b"b", b"c"):
                    monitor.sendall(b"sendkey " + key + b"\n")
                    time.sleep(0.4)
                pump(0.5)
            deadline = time.time() + args.timeout
            failed = args.expect.replace(" ok", " FAILED")
            while time.time() < deadline and args.expect.encode() not in log and failed.encode() not in log:
                pump(0.5)
            pump(1.0)
            text = log.decode("utf-8", "replace")
            print(text[-6000:])
            return 0 if args.expect in text else 1
        monitor = connect(args.monitor_port)
        qmp = connect(args.qmp_port)
        qmp.settimeout(2)
        qmp.recv(65536)
        qmp.sendall(b'{"execute":"qmp_capabilities"}\n')
        time.sleep(0.3)
        qmp.recv(65536)
        time.sleep(0.7)
        for key in (b"a", b"b", b"c"):
            monitor.sendall(b"sendkey " + key + b"\n")
            time.sleep(0.4)
        qmp.sendall(
            b'{"execute":"input-send-event","arguments":{"events":['
            b'{"type":"abs","data":{"axis":"x","value":4096}},'
            b'{"type":"abs","data":{"axis":"y","value":8192}}]}}\n'
        )
        time.sleep(0.3)
        qmp.recv(65536)

        # The probe drains the queue, clears O_NONBLOCK and blocks in read;
        # release it only once it has printed the marker.
        deadline = time.time() + args.timeout
        while time.time() < deadline and b"blocking-ready" not in log and b"input FAILED" not in log:
            pump(0.5)
        if b"blocking-ready" in log:
            monitor.sendall(b"sendkey d\n")

        deadline = time.time() + args.timeout
        while time.time() < deadline and b"input ok" not in log and b"input FAILED" not in log:
            pump(0.5)
        pump(1.0)
        text = log.decode("utf-8", "replace")
        print(text[-6000:])
        result = "input ok" in text

        log.clear()
        serial.sendall(b"health\n")
        pump(3.0)
        serial.sendall(b"svc\n")
        pump(3.0)
        print("=== HEALTH/SVC ===")
        print(log.decode("utf-8", "replace")[-3000:])
        return 0 if result else 1
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


if __name__ == "__main__":
    sys.exit(main())
