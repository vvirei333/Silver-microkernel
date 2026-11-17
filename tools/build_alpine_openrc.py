#!/usr/bin/env python3
"""Install a real Alpine + OpenRC rootfs into an SFS image.

The Linux ABI's `/` is the SFS root, so this image boots like a
postmarketOS-style rootfs: `/sbin/init` is BusyBox init, `/etc/inittab` runs
OpenRC, and OpenRC's `default` runlevel starts the existing Phosh stack
through `/sfs/phosh-serve`.

The source image is the working Phosh image. It is copied, grown with
`scripts/sfs_resize.py`, and the Alpine tree is merged on top: Alpine files
win on collision (the Alpine layer is the base), while the Arch-staged Phosh
closure stays reachable under both `/` and the legacy `/sfs` alias.

Usage:
    tools/build_alpine_openrc.py                 # ->< /tmp/werewolf-alpine-openrc.img
    tools/build_alpine_openrc.py --force         # rebuild from the Phosh image
    tools/alpine_openrc_smoke.py

The Alpine stage is cached in /tmp/alpine-openrc; apk runs under qemu-user
through binfmt, so no root is needed. Package triggers that need user
namespaces fail on the host; this tool recreates the BusyBox applet symlinks
from `/etc/busybox-paths.d/busybox`, which is what the trigger would do.
"""
import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from sfs_inject import Image  # noqa: E402

APK = Path("/tmp/opencode/sbin/apk.static")
KEYS = Path("/tmp/opencode/alpine-root/etc/apk/keys")
STAGE = Path("/tmp/alpine-openrc")
DEFAULT_SOURCE = Path("/home/ivan/phosh-state/disk_phosh.img")
DEFAULT_TARGET = Path("/tmp/werewolf-alpine-openrc.img")
REPOSITORIES = (
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main",
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/community",
)
# VFS refuses a single file above 256 MiB (`vfs::MAX_FILE_BYTES`).
MAX_FILE = 256 * 1024 * 1024
# SFS directory entries store the name inline: 27 bytes.
NAME_MAX = 255

INITTAB = """# /etc/inittab
::sysinit:/sbin/boot-session

::shutdown:/sbin/openrc shutdown
"""

# OpenRC writing to the console hangs at exit on this ABI (docs/ALPINE_OPENRC.md),
# so sysinit's output goes to a log and the session service is the console
# owner. `exec` keeps the pid busybox init waits on.
BOOT_SESSION = """#!/bin/sh
/sbin/openrc sysinit >/tmp/openrc-sysinit.log 2>&1
exec /sbin/rc-service phosh start 2>/tmp/phosh-start.log
"""

PHOSH_SERVICE = """#!/sbin/openrc-run
# Phosh runs on the Silver Wayland surface served by /sfs/phosh-serve.
description="Phosh shell on the Silver Wayland compositor"
command="/sfs/phosh-serve"
command_background=false
"""


def run(command: list[str]) -> None:
    subprocess.run(command, check=True)


def build_stage(force: bool) -> None:
    if (STAGE / "sbin/openrc").exists() and not force:
        postprocess()
        return
    if not APK.exists():
        sys.exit(
            "missing apk.static at /tmp/opencode/sbin/apk.static; "
            "run tools/build_alpine_env.py once to populate /tmp/opencode"
        )
    if STAGE.exists():
        shutil.rmtree(STAGE)
    STAGE.mkdir(parents=True)
    command = [
        str(APK), "--usermode", "--root", str(STAGE), "--arch", "aarch64",
        "--initdb", "--no-progress", "--no-scripts",
        "--keys-dir", str(KEYS),
    ]
    for repository in REPOSITORIES:
        command += ["-X", repository]
    command += ["add", "alpine-base", "openrc"]
    run(command)
    postprocess()


def postprocess() -> None:
    """Recreate what the (usermode-blocked) apk triggers would install."""
    # Some packaged files are execute-only (bbsuid is 0111); this tool runs
    # unprivileged, so grant read for packing. SFS stores no mode bits.
    for directory, _, filenames in os.walk(STAGE):
        for name in filenames:
            path = Path(directory) / name
            if path.is_symlink():
                continue
            try:
                mode = path.stat().st_mode
                path.chmod(0o755 if mode & 0o111 else 0o644)
            except OSError:
                pass
    applets = STAGE / "etc/busybox-paths.d/busybox"
    if applets.exists():
        for line in applets.read_text().splitlines():
            name = line.strip()
            if not name:
                continue
            target = STAGE / name
            if target.exists() or target.is_symlink():
                continue
            if len(target.name.encode()) > NAME_MAX:
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            os.symlink("/bin/busybox", target)
    init = STAGE / "sbin/init"
    if not init.is_symlink():
        init.parent.mkdir(parents=True, exist_ok=True)
        init.unlink(missing_ok=True)
        os.symlink("/bin/busybox", init)

    for directory in (
        "dev", "proc", "sys", "run", "tmp", "root", "home", "mnt", "media",
        "srv", "opt", "usr/local", "var/log", "var/lib/misc", "var/tmp",
        "var/cache", "etc/runlevels/sysinit", "etc/runlevels/boot",
        "etc/runlevels/default", "etc/runlevels/shutdown", "etc/local.d",
    ):
        (STAGE / directory).mkdir(parents=True, exist_ok=True)

    (STAGE / "etc/apk/repositories").write_text(
        "\n".join(REPOSITORIES) + "\n"
    )
    (STAGE / "etc/apk/arch").write_text("aarch64\n")
    (STAGE / "etc/inittab").write_text(INITTAB)
    (STAGE / "sbin/boot-session").write_text(BOOT_SESSION)
    service = STAGE / "etc/init.d/phosh"
    service.write_text(PHOSH_SERVICE)
    service.chmod(0o755)
    for runlevel, name in (
        ("sysinit", "procfs"),
        ("sysinit", "sysfs"),
        ("default", "phosh"),
    ):
        link = STAGE / "etc/runlevels" / runlevel / name
        if not link.is_symlink():
            link.unlink(missing_ok=True)
            os.symlink(f"/etc/init.d/{name}", link)


def prepare_image(source: Path, target: Path, force: bool) -> None:
    if target.exists() and not force:
        return
    target.unlink(missing_ok=True)
    print("copy", source, "->", target)
    shutil.copyfile(source, target)
    print("grow to 3 GiB")
    run([
        sys.executable, str(ROOT / "scripts/sfs_resize.py"),
        str(target), "3072", "32768",
    ])


def pack(stage: Path, image: Image) -> tuple[int, int, list[str]]:
    files = links = 0
    skipped: list[str] = []
    for directory, dirnames, filenames in os.walk(stage):
        relative = os.path.relpath(directory, stage)
        prefix = "" if relative == "." else relative.replace(os.sep, "/")
        keep = []
        for name in dirnames:
            path = f"{prefix}/{name}" if prefix else name
            if len(name.encode()) > NAME_MAX:
                skipped.append(path)
                continue
            source = os.path.join(directory, name)
            if os.path.islink(source):
                image.write_link_at(path, os.readlink(source))
                links += 1
                continue
            image.mkdir(path)
            keep.append(name)
        dirnames[:] = keep
        for name in filenames:
            path = f"{prefix}/{name}" if prefix else name
            if len(name.encode()) > NAME_MAX:
                skipped.append(path)
                continue
            source = os.path.join(directory, name)
            if os.path.islink(source):
                image.write_link_at(path, os.readlink(source))
                links += 1
                continue
            payload = Path(source).read_bytes()
            if len(payload) > MAX_FILE:
                skipped.append(path + " (too large)")
                continue
            image.write_file_at(path, payload)
            files += 1
    return files, links, skipped


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default=str(DEFAULT_SOURCE))
    parser.add_argument("--target", default=str(DEFAULT_TARGET))
    parser.add_argument("--force", action="store_true",
                        help="rebuild the Alpine stage and the image copy")
    args = parser.parse_args()

    source = Path(args.source)
    target = Path(args.target)
    if not source.exists():
        sys.exit("missing source image: " + str(source))

    build_stage(args.force)
    prepare_image(source, target, args.force)
    image = Image(str(target))
    files, links, skipped = pack(STAGE, image)
    image.save()
    print(f"wrote {target}")
    print(f"  files {files}, symlinks {links}, skipped {len(skipped)}")
    for path in skipped[:20]:
        print("  skip", path)
    if len(skipped) > 20:
        print(f"  ... {len(skipped) - 20} more")
    print("boot: linuxrun /sfs/sbin/init")
    return 0


if __name__ == "__main__":
    sys.exit(main())
