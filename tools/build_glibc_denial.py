#!/usr/bin/env python3
"""Build an Alpine edge SFS image staged with a Debian arm64 glibc runtime
and the Denial desktop environment.

Denial's aarch64 release packages (Arch pkg.tar.zst from GitHub) are glibc
binaries: PT_INTERP /lib/ld-linux-aarch64.so.1, DT_NEEDED on libc.so.6,
libgbm.so.1, libseat.so.1, libudev.so.1, libinput.so.10, libxkbcommon.so.0.
Alpine has no glibc, so the runtime is pulled from Debian trixie
(tools/debian_repo.py) and unpacked into /usr/lib/aarch64-linux-gnu; the
Alpine edge base (busybox + openrc) keeps the proven boot chain.

LD_LIBRARY_PATH is applied per-process (seatd, deniald) so musl binaries
never see Debian libraries.

boot-session runs seatd, then deniald with the log on the console so the
first failure is visible on the serial line.

Usage:
    tools/build_glibc_denial.py
"""
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "tools"))

from sfs_inject import Image  # noqa: E402
import alpine_repo  # noqa: E402
import debian_repo  # noqa: E402
from build_alpine_phosh_direct import (  # noqa: E402
    data_tar, first_component_exists, member_path, unpack,
)

TARGET = ROOT / "build" / "werewolf-alpine-glibc-denial.img"
SIZE_MIB = 2048
INODES = 49152

DENIAL_PKGS = [
    ROOT / "build/denial/denial-0.6.7-1-aarch64.pkg.tar.zst",
    ROOT / "build/denial/denial-flutter-engine-1.0.6.7-1-aarch64.pkg.tar.zst",
]

# Alpine edge: boot chain only (musl land).
ALPINE_ROOTS = ["alpine-base", "openrc", "busybox", "bash"]

# Debian trixie arm64: the glibc runtime deniald resolves against.
DEBIAN_ROOTS = [
    "libc6", "libgcc-s1", "libstdc++6",
    "libgbm1", "libegl1", "libgl1-mesa-dri",
    "seatd", "libseat1", "libudev1",
    "libinput10", "libxkbcommon0",
    "libglib2.0-0t64", "libpolkit-agent-1-0", "libpolkit-gobject-1-0",
    "libfontconfig1",
]

# Paths we do not need in the image (inode/space hygiene).
SKIP_PREFIXES = (
    "usr/share/doc/", "usr/share/man/", "usr/share/locale/",
    "usr/share/lintian/", "usr/share/bug/", "usr/share/doc-base/",
)

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
export GBM_BACKENDS_PATH=/usr/lib/aarch64-linux-gnu
export LIBSEAT_BACKEND=seatd

# seatd owns the DRM/input device handoff for libseat's seat backend.
# Debian ships seatd in /usr/sbin; plain mkdir (no -p) avoids the busybox
# mkdir -p stat('/') quirk against the SFS root.
mkdir /run/seatd 2>/dev/null
LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu /usr/sbin/seatd -g root >/tmp/seatd.log 2>&1 &
sleep 1

echo "DENIAL: starting deniald"
# --flutter-bundle switches runtime_limit() from TestOnly to UntilLogout:
# without it deniald exits right after the diagnostic scanout test.
export RUST_LOG=debug
# The console drops bytes under load; the file keeps the full stderr and the
# quiet `cat` after exit replays it over serial.
LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu /usr/bin/deniald --flutter-bundle /usr/lib/denial/flutter --wayland --no-xwayland > /tmp/deniald.log 2>&1
echo "DENIAL: deniald exited ($?)"
# Let the abort/fault console flood subside before replaying stderr.
sleep 2
cat /tmp/deniald.log
echo "DENIAL: log end"
"""


def main() -> int:
    for pkg in DENIAL_PKGS:
        if not pkg.exists():
            sys.exit(f"missing {pkg}")

    print("fetching APKINDEX (edge) ...")
    alpine_index = alpine_repo.Index()
    alpine_packages = alpine_index.closure(ALPINE_ROOTS)
    print(f"{len(alpine_packages)} alpine packages in the closure")

    debian_index = debian_repo.Index()
    debian_packages = debian_index.closure(DEBIAN_ROOTS)
    print(f"{len(debian_packages)} debian packages in the closure")

    print("creating image ...")
    TARGET.unlink(missing_ok=True)
    subprocess.run([
        sys.executable, str(ROOT / "scripts/make_sfs.py"),
        str(TARGET), str(SIZE_MIB), str(INODES),
    ], check=True)
    image = Image(str(TARGET))
    files = links = 0

    CACHE = ROOT / "build" / "apk-cache"
    CACHE.mkdir(parents=True, exist_ok=True)
    for position, record in enumerate(alpine_packages):
        path = alpine_repo.download(record, CACHE)
        f, l = unpack(record, path, image)
        files += f
        links += l
        if position % 50 == 0:
            print(f"  alpine {position}/{len(alpine_packages)} {record['P']}")

    DEB_CACHE = ROOT / "build" / "deb-cache"
    DEB_CACHE.mkdir(parents=True, exist_ok=True)
    hardlinks: dict[str, bytes] = {}
    for position, record in enumerate(debian_packages):
        path = debian_repo.download(record, DEB_CACHE)
        print(f"  debian {position}/{len(debian_packages)} "
              f"{record['Package']} ({path.stat().st_size // 1024} KiB)")
        with debian_repo.deb_data_tar(path) as archive:
            for member in archive.getmembers():
                destination = member_path(member.name)
                if destination == "/":
                    continue
                if any(destination.startswith(p) for p in SKIP_PREFIXES):
                    continue
                if member.isdir():
                    image.mkdir(destination)
                elif member.issym():
                    image.write_link_at(destination, member.linkname)
                    links += 1
                elif member.islnk():
                    content = hardlinks.get(member.linkname)
                    if content is not None:
                        image.write_file_at(destination, content)
                        files += 1
                elif member.isfile():
                    payload = archive.extractfile(member).read()
                    hardlinks[destination] = payload
                    image.write_file_at(destination, payload)
                    files += 1

    # denial release packages: unpack every file they carry
    for pkg in DENIAL_PKGS:
        print(f"unpacking {pkg.name}")
        with denial_tar(pkg) as archive:
            for member in archive.getmembers():
                if member.name in (".PKGINFO", ".MTREE", ".BUILDINFO"):
                    continue
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

    # glibc loader: PT_INTERP is /lib/ld-linux-aarch64.so.1, the file lives
    # in the Debian multiarch directory.
    image.write_link_at("/lib/ld-linux-aarch64.so.1",
                        "/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1")
    links += 1
    image.write_file_at("/etc/ld.so.conf",
                        b"/usr/lib/aarch64-linux-gnu\n"
                        b"/usr/local/lib/aarch64-linux-gnu\n")

    # postprocess (same shape as the other denial builder)
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
    # busybox applet links: unpack() captured the applet list when it wrote
    # /etc/busybox-paths.d/busybox; replay it like apk's trigger would.
    links_added = 0
    for line in unpack.applets.splitlines():
        name = line.strip()
        if not name:
            continue
        if first_component_exists(image, name.strip("/")):
            continue
        image.write_link_at("/" + name.lstrip("/"), "/bin/busybox")
        links_added += 1
    init = image.find_entry(image.root, "sbin")
    target = image.find_entry(init, "init")
    if target is None or image.inode(target)[0] != 3:
        image.write_link_at("/sbin/init", "/bin/busybox")
    image.save()
    print(f"image {TARGET} ({TARGET.stat().st_size // (1024 * 1024)} MiB)")
    print(f"  files {files}, symlinks {links + links_added}")
    return 0


def denial_tar(pkg: Path):
    """Open an Arch package (tar.zst) fully decompressed (seekable)."""
    import compression.zstd
    import io
    import tarfile
    decompressed = compression.zstd.decompress(pkg.read_bytes())
    return tarfile.open(fileobj=io.BytesIO(decompressed), mode="r:")


def read_file_bytes(image, inode):
    kind, size, blocks, indirect, double, triple = image.inode(inode)
    data = bytearray()
    for block in image.block_list(blocks, indirect, double, triple):
        off = image.block_offset(block)
        data.extend(image.data[off:off+512])
    return bytes(data[:size])


if __name__ == "__main__":
    sys.exit(main())
