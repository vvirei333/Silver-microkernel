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
export GBM_BACKENDS_PATH=/usr/lib/aarch64-linux-gnu/gbm:/usr/lib/aarch64-linux-gnu
export LIBSEAT_BACKEND=seatd

# seatd owns the DRM/input device handoff for libseat's seat backend.
# Debian ships seatd in /usr/sbin; plain mkdir (no -p) avoids the busybox
# mkdir -p stat('/') quirk against the SFS root.
mkdir /run/seatd 2>/dev/null
LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu /usr/sbin/seatd -g root >/tmp/seatd.log 2>&1 &
sleep 1

# One-shot file-mmap validation BEFORE deniald: the flutter engine reads
# icudtl.dat through a private file mapping; print what the guest actually
# sees. Runs first so the result is never swallowed by an engine hang.
echo "ICU-TEST begin"
/sfs/test-icu
echo "ICU-TEST end"
# Whole-file lazy-fill probe: deniald's leader died on a page fault reading
# ~0x3f1ec into a file mapping; touch every page of icudtl.dat.
echo "ICUPROBE begin"
/sfs/icuprobe
echo "ICUPROBE end ($?)"

echo "DENIAL: starting deniald"
# External Wayland client probe: commits a shm buffer against deniald's own
# compositor to see whether the render pipeline reacts to a client at all.
# External Wayland client probe: retries every 3 s so the freshest
# accept/close sequence is always recent in the kernel trace ring.
( sleep 45; echo "WL-CLIENT begin"; i=0; while [ $i -lt 30 ]; do /sfs/wl-client && break; i=$((i+1)); sleep 3; done; echo "WL-CLIENT end ($?)" ) &
# Live replay of the stderr log while the session runs: the log file holds
# the tracing output, the console drops bytes, so a delayed cat bridges them.
( sleep 90; echo "DENIAL: --- log snapshot ---"; cat /tmp/deniald.log ) &
# The engine keeps running past the first snapshot; periodic tails catch the
# failure line even when the final post-exit replay never reaches the
# byte-starved console. session_lock retries retry every 2s and drown the
# interesting lines, so they are filtered out.
( sleep 100; echo "DENIAL: --- tail 100 ---"; tail -c 20000 /tmp/deniald.log | grep -v session_lock | tail -c 2500 ) &
( sleep 130; echo "DENIAL: --- tail 130 ---"; tail -c 20000 /tmp/deniald.log | grep -v session_lock | tail -c 2500 ) &
( sleep 160; echo "DENIAL: --- tail 160 ---"; tail -c 20000 /tmp/deniald.log | grep -v session_lock | tail -c 2500 ) &
( sleep 190; echo "DENIAL: --- tail 190 ---"; tail -c 20000 /tmp/deniald.log | grep -v session_lock | tail -c 2500 ) &
# --flutter-bundle switches runtime_limit() from TestOnly to UntilLogout:
# without it deniald exits right after the diagnostic scanout test.
export RUST_LOG=debug
export MESA_DEBUG=1
export LIBGL_DEBUG=verbose
# llvmpipe sizes its per-thread JIT/Gallium arenas by the core count it sees
# (~193 MiB per thread); with 8 CPUs that exhausts the guest mmap window and
# the transfer worker dies on a NULL deref. Two threads fit comfortably.
export LP_NUM_THREADS=1
# The console drops bytes under load; the file keeps the full stderr and the
# quiet `cat` after exit replays it over serial.
LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu /usr/bin/deniald --flutter-bundle /usr/lib/denial/flutter --wayland --no-xwayland --software-rendering > /tmp/deniald.log 2>&1
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

    # libsystemd shim: the real library asserts deep inside its helpers when
    # systemd is absent (isatty_safe, fds_are_same_mount) and aborts the
    # compositor. Only libseat1 and libpolkit-gobject import sd_* symbols
    # (26 total); replace the .so with a freestanding stub that answers
    # "no systemd available" for all of them.
    shim = ROOT / "build/analysis/libsystemd-shim.so.0"
    if shim.exists():
        image.write_file_at("/usr/lib/aarch64-linux-gnu/libsystemd.so.0.40.0",
                            shim.read_bytes())
        print("  libsystemd replaced with the stub shim")
    else:
        print("  WARNING: libsystemd shim missing, keeping the Debian library")

    # libudev.so.1 embeds the same systemd basic sources statically (Debian
    # builds it self-contained), so its own fds_are_same_mount asserts on a
    # negative mount id and aborts the compositor. NOP the two conditional
    # branches into the assert blocks; the function then compares whatever
    # ids it collected, which is meaningless on this single-filesystem view.
    udev_path = "/usr/lib/aarch64-linux-gnu/libudev.so.1.7.10"
    payload = hardlinks.get(udev_path)
    if payload is not None:
        patched = bytearray(payload)
        for offset, expected in ((0x21E94, 0x37F80781), (0x21F14, 0x37F804A0)):
            actual = int.from_bytes(patched[offset:offset + 4], "little")
            if actual != expected:
                sys.exit(f"libudev patch site mismatch at {offset:#x}: "
                         f"{actual:#x} != {expected:#x}")
            patched[offset:offset + 4] = (0xD503201F).to_bytes(4, "little")
        image.write_file_at(udev_path, bytes(patched))
        print("  libudev fds_are_same_mount asserts NOPed")
    else:
        print("  WARNING: libudev payload not found, asserts stay active")

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
    # Account database: deniald's authentication module retries passwd -S in
    # a loop while "account metadata is unavailable", flooding the console
    # with passwd spawn noise. A minimal root/uid-5000 user closes the loop.
    image.write_file_at("/etc/passwd",
                        b"root:x:0:0:root:/root:/bin/sh\n"
                        b"user:x:5000:5000:user:/home/user:/bin/sh\n")
    image.write_file_at("/etc/shadow",
                        b"root:*:20000:0:99999:7:::\n"
                        b"user:*:20000:0:99999:7:::\n")
    image.write_file_at("/etc/group", b"root:x:0:\nuser:x:5000:\n")
    # The guest has no D-Bus, so deniald's account-metadata fallback spawns
    # passwd in an endless retry loop and floods the console. Answer with a
    # plausible `passwd -S user` line and stop the loop.
    image.write_file_at("/usr/bin/passwd",
                        b"#!/bin/sh\n"
                        b'echo "user P 11/21/2026 0 99999 7 -1"\n'
                        b"exit 0\n")
    icu_test = ROOT / "build/testbin/test_icu"
    # /sfs in the guest is a kernel alias for the image root, so the file
    # goes to the image root and is reached as /sfs/test-icu.
    if icu_test.exists():
        image.write_file_at("/test-icu", icu_test.read_bytes())
        print("  test binary staged at /sfs/test-icu (image /test-icu)")
    icu_probe = ROOT / "build/testbin/icuprobe"
    if icu_probe.exists():
        image.write_file_at("/icuprobe", icu_probe.read_bytes())
        print("  icuprobe staged at /sfs/icuprobe")
    wl_client = ROOT / "build/testbin/wl-client"
    if wl_client.exists():
        image.write_file_at("/wl-client", wl_client.read_bytes())
        print("  wl-client staged at /sfs/wl-client")
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
