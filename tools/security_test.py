#!/usr/bin/env python3
"""Run the security regression suite in the guest.

Every file in tests/security/*.c is a freestanding Linux ABI program. The
runner compiles it, installs it into a *copy* of disk.img, boots the release
kernel in QEMU and runs each case through `linuxrun`, requiring the expected
marker in the output.

The tracked disk.img is never modified; the copy lives in /tmp.

Usage:
    tools/security_test.py                 # build, inject, boot, run
    tools/security_test.py --no-boot       # compile and inject only
    tools/security_test.py --case fd_lifetime
"""
import argparse
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
WORK_IMAGE = Path("/tmp/werewolf-security.img")
SERIAL = "/tmp/werewolf-security.serial"
MONITOR = "/tmp/werewolf-security.monitor"

# file stem -> marker the guest must print
CASES = {
    "fd_lifetime": b"fd lifetime ok",
    "sigreturn_pstate": b"sigreturn pstate blocked",
    "sigreturn_aa32": b"sigreturn aa32 blocked",
    "sigreturn_flags": b"sigreturn flags ok",
}


class Guest:
    def __init__(self, image):
        self.image = image
        self.proc = None
        self.sock = None
        self.log = bytearray()

    def boot(self):
        for path in (SERIAL, MONITOR):
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
        self.proc = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
            "-smp", "2", "-m", "1024M",
            "-kernel", str(KERNEL),
            "-drive", f"file={self.image},format=raw,if=none,id=hd",
            "-device", "virtio-blk-pci,drive=hd",
            "-device", "virtio-net-pci,disable-legacy=on",
            "-display", "none",
            "-serial", f"unix:{SERIAL},server=on,wait=off",
            "-monitor", f"unix:{MONITOR},server=on,wait=off",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(300):
            try:
                sock = socket.socket(socket.AF_UNIX)
                sock.connect(SERIAL)
                self.sock = sock
                break
            except OSError:
                time.sleep(0.1)
        if self.sock is None:
            raise SystemExit("serial socket did not appear")
        self.sock.settimeout(0.2)

    def wait(self, marker, timeout=120):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                return False
            self.log += data
            if marker in self.log:
                return True
        return False

    def console(self, command):
        self.sock.sendall((command + "\n").encode())

    def run(self, name, marker, timeout=90):
        self.log.clear()
        self.console("linuxrun /sfs/sec-" + name)
        return self.wait(marker, timeout)

    def stop(self):
        if self.sock is not None:
            self.sock.close()
            self.sock = None
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def run_fuzz():
    source = ROOT / "tests/security/fuzz/http_parse_fuzz.rs"
    binary = Path("/tmp/werewolf-http-fuzz")
    subprocess.run(["rustc", "--edition", "2021", "-O", "-o", str(binary),
                    str(source)], check=True)
    result = subprocess.run([str(binary)])
    return result.returncode == 0


def build_cases():
    out = ROOT / "build/security"
    out.mkdir(parents=True, exist_ok=True)
    cc = os.environ.get("LINUX_CC", "aarch64-linux-gnu-gcc")
    built = []
    for source in sorted((ROOT / "tests/security").glob("*.c")):
        name = source.stem
        target = out / (name + ".elf")
        command = [
            cc, "-static", "-nostdlib", "-ffreestanding", "-O2",
            "-Wall", "-Wextra",
            "-Wl,-T," + str(ROOT / "tests/security/test.ld"),
            "-Wl,-z,max-page-size=4096",
            "-o", str(target), str(source),
        ]
        subprocess.run(command, check=True)
        built.append((name, target))
    return built


def prepare_image(built):
    if not (ROOT / "disk.img").exists():
        raise SystemExit("disk.img missing")
    shutil.copyfile(ROOT / "disk.img", WORK_IMAGE)
    for name, target in built:
        subprocess.run([sys.executable, str(ROOT / "scripts/sfs_inject.py"),
                        str(WORK_IMAGE), str(target), "sec-" + name],
                       check=True, stdout=subprocess.DEVNULL)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", help="run only this case")
    parser.add_argument("--no-boot", action="store_true",
                        help="compile and inject, do not boot")
    parser.add_argument("--fuzz", action="store_true",
                        help="also run the host-side HTTP parser fuzzer")
    args = parser.parse_args()

    if not KERNEL.exists():
        raise SystemExit("build first: ./build_and_qemu.sh release --no-run")

    built = build_cases()
    if not built:
        raise SystemExit("no tests/security/*.c found")
    print("built: " + ", ".join(name for name, _ in built))

    selected = [(name, target) for name, target in built
                if args.case in (None, name)]
    if not selected:
        raise SystemExit("no such case: " + str(args.case))
    missing = [name for name, _ in selected if name not in CASES]
    if missing:
        raise SystemExit("no expected marker for: " + ", ".join(missing))

    if args.fuzz:
        if not run_fuzz():
            sys.exit(1)

    prepare_image(built)
    if args.no_boot:
        print("image ready: %s" % WORK_IMAGE)
        return

    guest = Guest(WORK_IMAGE)
    failed = []
    try:
        guest.boot()
        if not guest.wait(b"howl>", 120):
            raise SystemExit("no howl prompt")
        for name, _ in selected:
            ok = guest.run(name, CASES[name])
            print(("PASS " if ok else "FAIL ") + name, flush=True)
            if not ok:
                failed.append(name)
    finally:
        guest.stop()

    print("\n%d/%d security cases passed" %
          (len(selected) - len(failed), len(selected)))
    if failed:
        print("failed: " + ", ".join(failed))
        sys.exit(1)


if __name__ == "__main__":
    main()
