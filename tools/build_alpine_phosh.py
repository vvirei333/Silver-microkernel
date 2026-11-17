#!/usr/bin/env python3
"""Build an Alpine-only SFS image with Phosh/phoc from the Alpine packages.

Unlike `build_alpine_openrc.py` this image does not merge the Arch-staged
closure: the root is the Alpine stage itself (busybox, OpenRC, phosh, phoc and
their dependencies). It boots busybox init -> boot-session -> OpenRC sysinit ->
phoc (headless, pixman) with phosh as its shell.

Usage:
    tools/build_alpine_phosh.py            # stage must already be installed
    tools/build_alpine_phosh.py --install  # apk add phosh phoc alpine-base openrc

The stage is /tmp/alpine-phosh; the image is /tmp/werewolf-alpine-phosh.img
(2 GiB). Nothing tracked is modified.
"""
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "tools"))

from sfs_inject import Image  # noqa: E402
import build_alpine_openrc as base  # noqa: E402

STAGE = Path("/tmp/alpine-phosh")
APK = Path("/tmp/opencode/sbin/apk.static")
KEYS = Path("/tmp/opencode/alpine-root/etc/apk/keys")
TARGET = Path("/tmp/werewolf-alpine-phosh.img")
SIZE_MIB = 2048
INODES = 32768

REPOSITORIES = [
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main",
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/community",
]

INITTAB = """# /etc/inittab
::sysinit:/sbin/boot-session

::shutdown:/sbin/openrc shutdown
"""

# phoc is the compositor; phosh connects to it as the shell. The headless
# backend needs no DRM and pixman keeps rendering off the GPU, which this
# kernel does not expose to Linux tasks yet. OpenRC output goes to a log
# because a console-writing OpenRC process does not return (ALPINE_OPENRC.md).
BOOT_SESSION = """#!/bin/sh
/sbin/openrc sysinit >/tmp/openrc-sysinit.log 2>&1

export XDG_RUNTIME_DIR=/tmp
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman
export WLR_LIBINPUT_NO_DEVICES=1
export LIBSEAT_BACKEND=builtin
export XDG_SESSION_TYPE=wayland
export GDK_BACKEND=wayland
export GSK_RENDERER=cairo
export XDG_DATA_DIRS=/usr/share
export XDG_CONFIG_HOME=/tmp/.config
export XDG_CACHE_HOME=/tmp/.cache
mkdir /tmp/.config 2>/dev/null
mkdir /tmp/.cache 2>/dev/null
mkdir /tmp/gschemas 2>/dev/null
chmod 700 /tmp
# The apk glib trigger cannot run on the host; compile the schemas here with
# Alpine's own tools. Without gschemas.compiled phoc aborts at startup.
if [ -d /usr/share/glib-2.0/schemas ]; then
    cp /usr/share/glib-2.0/schemas/* /tmp/gschemas/ 2>/dev/null
    glib-compile-schemas /tmp/gschemas >/tmp/gschemas.log 2>&1
    export GSETTINGS_SCHEMA_DIR=/tmp/gschemas
fi

# A session bus is required by GSettings/dbus clients in the shell.
if command -v dbus-launch >/dev/null 2>&1; then
    eval "$(dbus-launch --sh-syntax)"
fi

# Prove the compositor serves clients: the socket appears after startup.
( sleep 45; if ls /tmp | grep -q wayland-0; then echo "WAYLAND SOCKET OK"; \
  else echo "WAYLAND SOCKET MISSING"; ls /tmp; fi ) &
exec /usr/bin/phoc -S -E /usr/bin/phosh
"""


def install() -> None:
    if not APK.exists():
        sys.exit("missing apk.static; run tools/build_alpine_env.py once")
    if STAGE.exists() and (STAGE / "usr/bin/phoc").exists():
        print("stage already has phoc")
        return
    if STAGE.exists():
        shutil.rmtree(STAGE)
    STAGE.mkdir(parents=True)
    command = [
        str(APK), "--usermode", "--root", str(STAGE), "--arch", "aarch64",
        "--initdb", "--no-progress", "--no-scripts", "--keys-dir", str(KEYS),
    ]
    for repository in REPOSITORIES:
        command += ["-X", repository]
    command += ["add", "alpine-base", "openrc", "phosh", "phoc", "dbus"]
    subprocess.run(command, check=True)


def postprocess() -> None:
    # Execute-only packaged files (bbsuid) cannot be read by the packer.
    for directory, _, filenames in os.walk(STAGE):
        for name in filenames:
            path = os.path.join(directory, name)
            if os.path.islink(path):
                continue
            try:
                os.chmod(path, os.stat(path).st_mode | 0o444)
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
            target.parent.mkdir(parents=True, exist_ok=True)
            os.symlink("/bin/busybox", target)
    init = STAGE / "sbin/init"
    if not init.is_symlink():
        init.parent.mkdir(parents=True, exist_ok=True)
        init.unlink(missing_ok=True)
        os.symlink("/bin/busybox", init)
    for directory in (
        "tmp", "run", "var", "var/cache", "var/cache/apk",
        "etc/runlevels/sysinit", "etc/runlevels/boot",
        "etc/runlevels/default", "etc/runlevels/shutdown", "etc/local.d",
        "etc/apk",
    ):
        (STAGE / directory).mkdir(parents=True, exist_ok=True)
    (STAGE / "etc/inittab").write_text(INITTAB)
    (STAGE / "sbin/boot-session").write_text(BOOT_SESSION)
    (STAGE / "etc/apk/repositories").write_text("\n".join(REPOSITORIES) + "\n")
    (STAGE / "etc/apk/arch").write_text("aarch64\n")
    # apk triggers are skipped (--no-scripts); the glib schema trigger is
    # replayed in the guest (boot-session) because qemu-user cannot relocate
    # Alpine's musl libgio.
    machine_id = "6f1c2b9e8a4d47c3b5e0a1f2d3c4b5a6"
    (STAGE / "etc/machine-id").write_text(machine_id + "\n")
    (STAGE / "var/lib/dbus").mkdir(parents=True, exist_ok=True)
    (STAGE / "var/lib/dbus/machine-id").write_text(machine_id + "\n")


def main() -> int:
    if "--install" in sys.argv or not (STAGE / "usr/bin/phoc").exists():
        install()
    postprocess()

    TARGET.unlink(missing_ok=True)
    subprocess.run([
        sys.executable, str(ROOT / "scripts/make_sfs.py"),
        str(TARGET), str(SIZE_MIB), str(INODES),
    ], check=True)
    image = Image(str(TARGET))
    files, links, skipped = base.pack(STAGE, image)
    image.save()
    size = TARGET.stat().st_size
    print(f"image {TARGET} ({size // (1024 * 1024)} MiB)")
    print(f"  files {files}, symlinks {links}, skipped {len(skipped)}")
    for path in skipped[:20]:
        print("  skip", path)
    if len(skipped) > 20:
        print(f"  ... {len(skipped) - 20} more")
    print("boot: linuxrun /sfs/sbin/init")
    return 0


if __name__ == "__main__":
    sys.exit(main())
