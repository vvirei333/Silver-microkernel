#!/usr/bin/env python3
"""Drive the guest over TCP serial with pacing, printing each reply.

Usage:
    python tools/serial_probe.py --image build/foo.img --cmd "run /sfs/bin/ls /tmp"
"""
import argparse
import time
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))

from qemu_serial import boot, Serial, KERNEL, ROOT  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, default=KERNEL)
    parser.add_argument("--memory", default="1024M")
    parser.add_argument("--cmd", action="append", default=[])
    parser.add_argument("--wait", action="append", default=[],
                        help="marker awaited after the previous command")
    parser.add_argument("--pause", type=float, default=4.0)
    parser.add_argument("--timeout", type=float, default=60.0)
    args = parser.parse_args()

    proc = boot(args.image, args.kernel, args.memory, [])
    try:
        serial = Serial()
        if not serial.wait(b"howl>", 120):
            print("BOOT FAILED")
            print(serial.tail())
            return 1
        waits = args.wait + [""] * len(args.cmd)
        for command, marker in zip(args.cmd, waits):
            if marker:
                serial.wait(marker.encode(), args.timeout)
            serial.log.clear()
            serial.send(command)
            time.sleep(args.pause)
            serial.pump()
            print(f"--- CMD: {command}")
            print(serial.tail(6000))
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
