#!/usr/bin/env python3
"""Build an Alpine edge SFS image with the Denial desktop environment.

Denial (denialwm) ships signed Alpine APKs on GitHub releases: deniald is a
static musl binary with DT_NEEDED on libgcompat (glibc shim), mesa's libgbm,
libseat, libudev, libinput, libxkbcommon, libpulse, linux-pam and ddcutil.
This builder installs those dependency packages from the Alpine edge
repositories through the regular resolver, then unpacks the denial APKs and
the Flutter engine straight into the image (same unpack path as
build_alpine_phosh_direct.py).

boot-session runs seatd, then deniald with the log on the console so the
first failure is visible on the serial line.

Usage:
    tools/build_alpine_denial.py
"""
import io
import subprocess
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "tools"))

from sfs_inject import Image  # noqa: E402
import alpine_repo  # noqa: E402
from build_alpine_phosh_direct import data_tar, member_path, unpack  # noqa: E402

TARGET = ROOT / "build" / "werewolf-alpine-denial.img"
SIZE_MIB = 2048
INODES = 32768

DENIAL_APKS = [
    ROOT / "build/denial/denial-0.6.7-r1.apk",
    ROOT / "build/denial/denial-flutter-engine-0.6.7-r1.apk",
]

# Everything deniald's DT_NEEDED (and its helpers) resolve to on Alpine edge.
DEP_ROOTS = [
    "gcompat", "seatd", "eudev", "mesa-egl", "mesa-gbm", "mesa",
    "libinput", "libxkbcommon", "pulseaudio", "linux-pam", "ddcutil",
    "dbus",
]

MACHINE_ID = "6f1c2b9e8a4d47c3b5e0a1f2d3c4b5a6"

INITTAB = """# /etc/inittab
::sysinit:/sbin/boot-session

::shutdown:/sbin/openrc shutdown
"""

BOOT_SESSION = """#!/bin/sh
/sbin/openrc sysinit >/tmp/openrc-sysinit.log 2>&1

export XDG_RUNTIME_DIR=/tmp
mkdir /tmp/.config 2>/dev/null
mkdir /tmp/.cache 2>/dev/null
chmod 700 /tmp
export LD_LIBRARY_PATH=/usr/lib/denial/alpine:/usr/lib/denial/flutter/lib

# seatd owns the DRM/input device handoff for libseat's seat backend.
mkdir -p /run/seatd
seatd -g root >/tmp/seatd.log 2>&1 &
sleep 1

echo "DENIAL: starting deniald"
/usr/bin/deniald
echo "DENIAL: deniald exited ($?)"
( sleep 5; echo "--- dmesg-style check ---"; ls /dev/dri; ls /dev/input 2>/dev/null; echo "--- end ---" ) &
"""


def main() -> int:
    for apk in DENIAL_APKS:
        if not apk.exists():
            sys.exit(f"missing {apk}; download the denial release APKs first")

    CACHE = ROOT / "build" / "apk-cache"
    CACHE.mkdir(parents=True, exist_ok=True)
    print("fetching APKINDEX (edge) ...")
    index = alpine_repo.Index()
    packages = index.closure(["alpine-base", "openrc", "busybox"] + DEP_ROOTS)
    print(f"{len(packages)} packages in the closure")

    print("creating image ...")
    TARGET.unlink(missing_ok=True)
    subprocess.run([
        sys.executable, str(ROOT / "scripts/make_sfs.py"),
        str(TARGET), str(SIZE_MIB), str(INODES),
    ], check=True)
    image = Image(str(TARGET))
    files = links = 0
    for position, record in enumerate(packages):
        path = alpine_repo.download(record, CACHE)
        f, l = unpack(record, path, image)
        files += f
        links += l
        if position % 50 == 0:
            print(f"  {position}/{len(packages)} {record['P']}")
    # denial APKs: unpack every file they carry
    for apk in DENIAL_APKS:
        print(f"unpacking {apk.name}")
        with data_tar(apk) as archive:
            for member in archive.getmembers():
                destination = member_path(member.name)
                if destination == "/" or len(member.name.split("/")[-1].encode()) > 255:
                    continue
                if member.isdir():
                    image.mkdir(destination)
                elif member.issym():
                    image.write_link_at(destination, member.linkname)
                    links += 1
                elif member.isfile():
                    payload = archive.extractfile(member).read()
                    image.write_file_at(destination, payload)
                    files += 1

    # postprocess (subset of the phosh builder's)
    for directory in ("dev", "proc", "sys", "run", "tmp", "root", "home",
                      "mnt", "media", "srv", "opt", "usr/local",
                      "var/log", "var/lib/misc", "var/tmp", "var/cache",
                      "var/lib/dbus", "etc/runlevels/sysinit",
                      "etc/runlevels/boot", "etc/runlevels/default",
                      "etc/runlevels/shutdown", "etc/local.d", "etc/apk"):
        image.mkdir(directory)
    image.write_file_at("/etc/inittab", INITTAB.encode())
    image.write_file_at("/sbin/boot-session", BOOT_SESSION.encode())
    image.write_file_at(
        "/etc/apk/repositories",
        ("\n".join(alpine_repo.REPOSITORIES) + "\n").encode())
    image.write_file_at("/etc/apk/arch", b"aarch64\n")
    image.write_file_at("/etc/machine-id", (MACHINE_ID + "\n").encode())
    image.write_file_at("/var/lib/dbus/machine-id", (MACHINE_ID + "\n").encode())
    # busybox applets + /sbin/init
    applets_path = image.find_entry(image.root, "etc")
    applets = image.find_entry(applets_path, "busybox-paths.d")
    links_added = 0
    if applets is not None:
        busybox_dir = image.find_entry(applets, "busybox")
        if busybox_dir is not None:
            content = read_file_bytes(image, busybox_dir)
            for line in content.decode("utf-8", "replace").splitlines():
                name = line.strip()
                if not name or "/" in name:
                    continue
                existing = image.find_entry(image.root, name)
                if existing is not None:
                    continue
                image.write_link_at("/" + name, "/bin/busybox")
                links_added += 1
    init = image.find_entry(image.root, "sbin")
    target = image.find_entry(init, "init")
    if target is None or image.inode(target)[0] != 3:
        image.write_link_at("/sbin/init", "/bin/busybox")
    image.save()
    print(f"image {TARGET} ({TARGET.stat().st_size // (1024 * 1024)} MiB)")
    print(f"  files {files}, symlinks {links + links_added}")
    return 0


def read_file_bytes(image, inode):
    kind, size, blocks, indirect, double, triple = image.inode(inode)
    data = bytearray()
    for block in image.block_list(blocks, indirect, double, triple):
        off = image.block_offset(block)
        data.extend(image.data[off:off+512])
    return bytes(data[:size])


if __name__ == "__main__":
    sys.exit(main())
