#!/usr/bin/env python3
"""Remove software-GL megadrivers from an SFS image.

`libEGL_mesa` pulls in `libgallium-*`, which in turn needs `libLLVM` and
`libz3`. That is ~400 MB of libraries, and the kernel maps every `PT_LOAD`
segment eagerly, so a GTK application paid 25-30 seconds before its first
frame just to copy pages of a JIT compiler it never executes. The desktop
image renders through `wl_shm` (Phosh is cairo-based and the compositor takes
shared-memory buffers), so it needs none of it. GL clients live in
`disk_gtk4.img`, which is built by `tools/install_mesa.py` and keeps them.

Usage: tools/prune_software_gl.py --image IMG [--dry-run]
"""

from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "scripts"))

from sfs_inject import Image  # noqa: E402

# Root-level names produced by the Mesa install. `libcrypto`, `libdrm` and the
# EGL/GLES entry points stay: removing them breaks every app at load time.
PATTERNS = ("libLLVM.so", "libgallium")


def matches(name: str) -> bool:
    return any(name.startswith(pattern) for pattern in PATTERNS)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, help="SFS image to modify")
    parser.add_argument("--dry-run", action="store_true",
                        help="list what would be removed")
    options = parser.parse_args()

    image = Image(options.image)
    removed = 0
    for block in image.dir_blocks(image.root):
        start = block * 512
        for slot in range(512 // 32):
            entry = start + slot * 32
            inode = int.from_bytes(image.data[entry:entry + 4], "little")
            if inode == 0:
                continue
            length = image.data[entry + 4]
            name = image.data[entry + 5:entry + 5 + length].decode()
            if not matches(name):
                continue
            size = image.inode(inode)[1]
            state = "would remove" if options.dry_run else "removing"
            print("%s %s (%d bytes)" % (state, name, size))
            removed += size
            if not options.dry_run:
                image.remove_file_at("/" + name)
    if not options.dry_run:
        image.save()
    print("%d bytes %s" % (removed, "would be freed" if options.dry_run
                           else "freed"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
