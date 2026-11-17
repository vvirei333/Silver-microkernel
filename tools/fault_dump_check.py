#!/usr/bin/env python3
"""Check the EL0 fault report: one atomic, self-identifying dump.

`tests/linux/regdump.c` faults with known sentinels in x0/x8/x19/x20. The
kernel must report those exact values, the faulting ELR and FAR, and the task
id in the *same* console write. A dump assembled from many small writes can
be interleaved by a second CPU that faults at the same time, which silently
attributes the wrong registers to the wrong task.

    tools/fault_dump_check.py
    tools/fault_dump_check.py --image /tmp/werewolf-test.img
"""
import argparse
import os
import re
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from sfs_inject import Image  # noqa: E402

KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
SERIAL = "/tmp/werewolf-faultdump.serial"
BINARY = ROOT / "build/regdump"


def build_binary() -> None:
    BINARY.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        os.environ.get("LINUX_CC", "aarch64-linux-gnu-gcc"),
        "-static", "-nostdlib", "-ffreestanding", "-O2",
        "-Wl,-T," + str(ROOT / "tests/linux/scope.ld"),
        "-Wl,-z,max-page-size=4096",
        "-o", str(BINARY), str(ROOT / "tests/linux/regdump.c"),
    ], check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", default=str(ROOT / "disk.img"))
    args = parser.parse_args()
    if not KERNEL.exists():
        sys.exit("build first: ./build_and_qemu.sh release --no-run")
    source = Path(args.image)
    if not source.exists():
        sys.exit("missing image: " + str(source))
    build_binary()

    image = Path("/tmp/opencode/disk-faultdump.img")
    shutil.copyfile(source, image)
    img = Image(str(image))
    img.write_file_at("/regdump", BINARY.read_bytes())
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
        sock.sendall(b"linuxrun /sfs/regdump\n")
        if not wait(b"FAULT task=", 60):
            sys.stderr.write(bytes(log)[-1500:].decode("utf-8", "replace"))
            print("FAIL no fault report")
            return 1
        wait(b"SP_EL0=", 5)
        text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", bytes(log).decode("utf-8", "replace"))
        header = re.search(
            r"FAULT task=(\d+) esr=0x([0-9a-f]+) far=0x([0-9a-f]+) elr=0x([0-9a-f]+)",
            text,
        )
        values = {name: int(value, 16) for name, value in re.findall(
            r"(x\d+)=0x([0-9a-f]+)", text
        )}
        checks = {
            "header": header is not None,
            "far null": header is not None and int(header.group(3), 16) == 0,
            "elr code": header is not None and int(header.group(4), 16) == 0x400010,
            "x0 null": values.get("x0") == 0,
            "x8 sentinel": values.get("x8") == 0x99,
            "x19 sentinel": values.get("x19") == 0x1234,
            "x20 sentinel": values.get("x20") == 0x5678,
        }
        failed = [name for name, ok in checks.items() if not ok]
        if failed:
            print("FAIL fault dump:", ", ".join(failed))
            print(text[-1200:])
            return 1
        print("PASS fault dump (task, registers, FAR and ELR in one report)")
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
