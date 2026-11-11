#!/usr/bin/env python3
"""Merge the aarch64 GTK4 runtime into a copy of the GTK3 SFS image."""
import hashlib
import importlib.util
import os
import shutil
import struct
import sys

CLOSURE = "/tmp/opencode/gtk4-closure.txt"
SOURCE = "/home/ivan/phosh-state/disk_gtk3.img"
TARGET = "/home/ivan/phosh-state/disk_gtk4.img"
EXTRA = {
    "gtk4hello": "/tmp/opencode/gtk4hello",
    "gschemas.compiled": (
        "/home/ivan/phosh-state/gtk4-root/usr/share/glib-2.0/schemas/"
        "gschemas.compiled"
    ),
}

spec = importlib.util.spec_from_file_location(
    "sfs", "/run/media/ivan/New Volume/My Projects/Silver microkernel/scripts/sfs_inject.py"
)
sfs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sfs)


def root_files(image):
    index = {}
    for block in image.dir_blocks(image.root):
        start = block * sfs.BLOCK
        for slot in range(sfs.BLOCK // 32):
            entry = start + slot * 32
            inode = struct.unpack_from("<I", image.data, entry)[0]
            if inode == 0:
                continue
            length = image.data[entry + 4]
            name = image.data[entry + 5 : entry + 5 + length].decode()
            index[name] = inode
    return index


def file_bytes(image, inode):
    kind, size, blocks, indirect, double, triple = image.inode(inode)
    if kind != sfs.INODE_KIND_FILE:
        return None
    payload = bytearray()
    for block in image.block_list(blocks, indirect, double, triple):
        payload += image.data[block * sfs.BLOCK : (block + 1) * sfs.BLOCK]
    return bytes(payload[:size])


def main(write):
    libraries = []
    for line in open(CLOSURE):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        _, name, _, path = line.split(None, 3)
        libraries.append((name, path))

    if not os.path.exists(TARGET):
        shutil.copyfile(SOURCE, TARGET)
    image = sfs.Image(TARGET)
    existing = root_files(image)
    additions = []
    skipped = 0
    for name, path in libraries:
        payload = open(path, "rb").read()
        if name in existing:
            current = file_bytes(image, existing[name])
            if current == payload:
                skipped += 1
                continue
        additions.append((name, payload))
    for name, path in EXTRA.items():
        additions.append((name, open(path, "rb").read()))

    total = sum(len(payload) for _, payload in additions)
    free = sum(
        1 for block in range(image.data_start, image.total) if not image.used(block)
    ) * sfs.BLOCK
    print(
        f"# add {len(additions)} files, {total} bytes "
        f"({total / 1048576:.1f} MiB); skip {skipped} identical; "
        f"free {free / 1048576:.1f} MiB"
    )
    if total > free:
        print("does not fit", file=sys.stderr)
        return 1
    if not write:
        return 0
    for name, payload in additions:
        image.write_file_at(name, payload)
    image.save()
    print("saved", TARGET)
    return 0


if __name__ == "__main__":
    sys.exit(main("--write" in sys.argv))
