#!/usr/bin/env python3
"""Build the Alpine-only Phosh SFS image without apk (Windows-safe).

Same output as tools/build_alpine_phosh.py: a 2 GiB SFS image whose root is
the Alpine stage (busybox init -> /sbin/boot-session -> OpenRC sysinit ->
headless phoc with phosh as the shell). apk.static needs qemu-user, which
does not exist on Windows, so packages are resolved with alpine_repo.py,
downloaded straight from dl-cdn (sha256-checked against APKINDEX) and their
data tars are unpacked directly into the SFS image.

Downloads are cached in build/apk-cache; nothing else on the host is touched.

Usage:
    tools/build_alpine_phosh_direct.py            # build /tmp/werewolf-alpine-phosh.img
    tools/build_alpine_phosh_direct.py --stage-only  # unpack into a host dir instead
"""
import argparse
import io
import os
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "tools"))

from sfs_inject import Image  # noqa: E402
import alpine_repo  # noqa: E402

CACHE = ROOT / "build" / "apk-cache"
TARGET = ROOT / "build" / "werewolf-alpine-phosh.img"
SIZE_MIB = 2048
INODES = 32768
MAX_FILE = 256 * 1024 * 1024  # vfs::MAX_FILE_BYTES
NAME_MAX = 255

ROOTS = ["alpine-base", "openrc", "phosh", "phoc", "dbus"]

MACHINE_ID = "6f1c2b9e8a4d47c3b5e0a1f2d3c4b5a6"

INITTAB = """# /etc/inittab
::sysinit:/sbin/boot-session

::shutdown:/sbin/openrc shutdown
"""

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

POST_DIRS = (
    "dev", "proc", "sys", "run", "tmp", "root", "home", "mnt", "media",
    "srv", "opt", "usr/local", "var/log", "var/lib/misc", "var/tmp",
    "var/cache", "var/cache/apk", "var/lib/dbus", "etc/runlevels/sysinit",
    "etc/runlevels/boot", "etc/runlevels/default", "etc/runlevels/shutdown",
    "etc/local.d", "etc/apk",
)


def data_tar(path: Path) -> tarfile.TarFile:
    """Open the payload tar of an .apk (the largest gzip member)."""
    members = alpine_repo.apk_data_members(path)
    biggest = max(members, key=len)
    return tarfile.open(fileobj=io.BytesIO(biggest), mode="r:")


def member_path(name: str) -> str:
    return "/" + name.lstrip("./").lstrip("/")


def unpack(record: dict, path: Path, image: Image) -> tuple[int, int]:
    files = links = 0
    with data_tar(path) as archive:
        for member in archive.getmembers():
            destination = member_path(member.name)
            if destination == "/":
                continue
            if len(member.name.split("/")[-1].encode()) > NAME_MAX:
                print("  skip name", member.name)
                continue
            if member.isdir():
                image.mkdir(destination)
            elif member.issym():
                image.write_link_at(destination, member.linkname)
                links += 1
            elif member.islnk():
                # hard link: copy the payload of the already-injected source
                source = archive.extractfile(member.linkname.lstrip("./"))
                if source is None:
                    print("  skip hardlink", member.name)
                    continue
                image.write_file_at(destination, source.read())
                files += 1
            elif member.isfile():
                payload = archive.extractfile(member).read()
                if len(payload) > MAX_FILE:
                    print("  skip too large", member.name)
                    continue
                image.write_file_at(destination, payload)
                files += 1
                # busybox ships its applet list; remember the content
                if destination == "/etc/busybox-paths.d/busybox":
                    unpack.applets = payload.decode("utf-8", "replace")
    return files, links


unpack.applets = ""


def postprocess(image: Image) -> int:
    """Replay apk triggers and seed the session config at the image level."""
    for directory in POST_DIRS:
        image.mkdir(directory)
    image.write_file_at("/etc/inittab", INITTAB.encode())
    image.write_file_at("/sbin/boot-session", BOOT_SESSION.encode())
    image.write_file_at(
        "/etc/apk/repositories",
        ("\n".join(alpine_repo.REPOSITORIES) + "\n").encode(),
    )
    image.write_file_at("/etc/apk/arch", b"aarch64\n")
    image.write_file_at("/etc/machine-id", (MACHINE_ID + "\n").encode())
    image.write_file_at(
        "/var/lib/dbus/machine-id", (MACHINE_ID + "\n").encode()
    )
    # /sbin/init must be busybox even if a package shipped a real file there
    init = image.find_entry(image.root, "sbin")
    target = image.find_entry(init, "init")
    if target is not None and image.inode(target)[0] != 3:  # not a symlink
        image.write_link_at("/sbin/init", "/bin/busybox")
    links = 0
    for line in unpack.applets.splitlines():
        name = line.strip()
        if not name:
            continue
        path = "/" + name.lstrip("/")
        existing = image.find_entry(image.root, name.strip("/").split("/")[0])
        # cheap existence check: walk only when the first component exists
        if first_component_exists(image, name.strip("/")):
            continue
        image.write_link_at(path, "/bin/busybox")
        links += 1
    return links


def first_component_exists(image: Image, relative: str) -> bool:
    parts = [piece for piece in relative.split("/") if piece]
    index = image.root
    for part in parts:
        child = image.find_entry(index, part)
        if child is None:
            return False
        index = child
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stage-only", action="store_true",
                        help="unpack into build/alpine-stage instead of an SFS")
    args = parser.parse_args()

    CACHE.mkdir(parents=True, exist_ok=True)
    print("fetching APKINDEX ...")
    index = alpine_repo.Index()
    packages = index.closure(ROOTS)
    print(f"{len(packages)} packages in the closure")

    if args.stage_only:
        stage = ROOT / "build" / "alpine-stage"
        stage.mkdir(parents=True, exist_ok=True)
        files = links = 0
        for record in packages:
            path = alpine_repo.download(record, CACHE)
            files += unpack_stage(record, path, stage)
        print(f"stage {stage}: {files} files")
        return 0

    print("creating image ...")
    import subprocess
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
    links += postprocess(image)
    image.save()
    print(f"image {TARGET} ({TARGET.stat().st_size // (1024 * 1024)} MiB)")
    print(f"  files {files}, symlinks {links}")
    print("boot: linuxrun /sfs/sbin/init")
    return 0


def unpack_stage(record: dict, path: Path, stage: Path) -> int:
    count = 0
    with data_tar(path) as archive:
        for member in archive.getmembers():
            if not member.isfile():
                continue
            destination = stage / member.name.lstrip("./")
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(archive.extractfile(member).read())
            count += 1
    return count


if __name__ == "__main__":
    sys.exit(main())
