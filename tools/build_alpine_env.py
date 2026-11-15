#!/usr/bin/env python3
"""Install a minimal Alpine (musl) userland into an SFS image copy.

The Linux ABI layer stays small; Alpine is the base the postmarketOS userspace
is built from. This first slice proves the smallest useful piece: the musl
dynamic loader plus BusyBox (`busybox`, `sh`) run as Linux EL0 tasks from
`/sfs`, with no glibc and no Arch staging.

Usage:
    tools/build_alpine_env.py                 # source disk.img -> /tmp/werewolf-alpine.img
    tools/build_alpine_env.py --image /tmp/x.img
    tools/alpine_smoke.py --image /tmp/x.img

The minirootfs is cached under /tmp/alpine-env; the tracked disk.img is never
modified.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from sfs_inject import Image  # noqa: E402

VERSION = "3.24.4"
URL = (
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/"
    f"alpine-minirootfs-{VERSION}-aarch64.tar.gz"
)
APK_VERSION = "3.0.9-r0"
APK_URL = (
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/"
    f"apk-tools-static-{APK_VERSION}.apk"
)
CACHE = Path("/tmp/alpine-env")
DEFAULT_SOURCE = ROOT / "disk.img"
DEFAULT_TARGET = Path("/tmp/werewolf-alpine.img")
FILES = (
    "./bin/busybox",
    "./lib/ld-musl-aarch64.so.1",
    "./etc/alpine-release",
    "./etc/os-release",
)


def fetch() -> Path:
    CACHE.mkdir(parents=True, exist_ok=True)
    archive = CACHE / f"alpine-minirootfs-{VERSION}-aarch64.tar.gz"
    if not archive.exists():
        print("download", URL)
        with urllib.request.urlopen(URL, timeout=120) as response:
            archive.write_bytes(response.read())
    return archive


def fetch_apk() -> Path:
    apk = CACHE / f"apk-tools-static-{APK_VERSION}.apk"
    if not apk.exists():
        print("download", APK_URL)
        with urllib.request.urlopen(APK_URL, timeout=120) as response:
            apk.write_bytes(response.read())
    return apk


def extract_apk(archive: Path) -> bytes:
    with tarfile.open(archive, "r:gz") as tar:
        for name in ("sbin/apk.static", "./sbin/apk.static"):
            try:
                member = tar.getmember(name)
            except KeyError:
                continue
            handle = tar.extractfile(member)
            if handle is not None:
                return handle.read()
    raise SystemExit("missing sbin/apk.static in apk-tools-static")


def extract(archive: Path) -> dict[str, bytes]:
    out: dict[str, bytes] = {}
    with tarfile.open(archive, "r:gz") as tar:
        for name in FILES:
            member = tar.getmember(name)
            handle = tar.extractfile(member)
            if handle is None:
                raise SystemExit("missing member: " + name)
            out[name] = handle.read()
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default=str(DEFAULT_SOURCE))
    parser.add_argument("--image", default=str(DEFAULT_TARGET))
    args = parser.parse_args()

    source = Path(args.source)
    target = Path(args.image)
    if not source.exists():
        sys.exit("missing source image: " + str(source))

    files = extract(fetch())
    busybox = files["./bin/busybox"]
    patched = CACHE / "busybox.patched"
    patched.write_bytes(busybox)
    subprocess.run(
        [
            "patchelf",
            "--set-interpreter",
            "/sfs/ld-musl-aarch64.so.1",
            "--set-rpath",
            "/sfs",
            str(patched),
        ],
        check=True,
    )
    busybox = patched.read_bytes()

    shutil.copyfile(source, target)
    image = Image(str(target))
    image.write_file_at("busybox", busybox)
    image.write_file_at("sh", busybox)
    # musl's loader is also libc; SFS has no symlinks, so install both names.
    loader = files["./lib/ld-musl-aarch64.so.1"]
    image.write_file_at("ld-musl-aarch64.so.1", loader)
    image.write_file_at("libc.musl-aarch64.so.1", loader)
    image.write_file_at("apk", extract_apk(fetch_apk()))
    if "./etc/alpine-release" in files:
        image.write_file_at("etc/alpine-release", files["./etc/alpine-release"])
    if "./etc/os-release" in files:
        image.write_file_at("etc/os-release", files["./etc/os-release"])
    image.save()
    print("wrote", target)
    print("  /sfs/busybox  (BusyBox v1.37, musl)")
    print("  /sfs/sh       (same binary, shell applet)")
    print("  /sfs/apk       (apk-tools 3.0.9, static)")
    print("  /sfs/ld-musl-aarch64.so.1 + /sfs/libc.musl-aarch64.so.1")
    print("next: tools/alpine_smoke.py --image", target)
    return 0


if __name__ == "__main__":
    sys.exit(main())
