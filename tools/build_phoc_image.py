#!/usr/bin/env python3
"""Install phoc (the postmarketOS compositor) into the Phosh SFS image.

This turns the image into the real stack: our mini compositor (phoc-serve)
hosts a nested wlroots compositor, and phoc starts Phosh as its shell.

Usage: tools/build_phoc_image.py [image]

Inputs (already staged by the session that fetched the packages):
  /tmp/alarm/x-phoc/usr/bin/phoc           extracted phoc package
  /tmp/alarm/phocroot/usr/lib              dependency closure (libwlroots etc.)
  build/phoc_serve.elf                     -DSERVE_ONLY -DSERVE_PHOC build
"""

import os
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                '..', 'scripts'))
from sfs_inject import Image  # noqa: E402

PHOC = '/tmp/alarm/x-phoc/usr/bin/phoc'
PHOC_LIBS = '/tmp/alarm/phocroot/usr/lib'
SERVE = 'build/phoc_serve.elf'
DEFAULT_IMAGE = os.path.expanduser('~/phosh-state/disk_phosh.img')

PHOC_INI = b'''[core]
# There is no XWayland server in the image; keep phoc from probing for one.
xwayland=false
'''


def needed(path):
    out = subprocess.check_output(['aarch64-linux-gnu-objdump', '-p', path],
                                  text=True, stderr=subprocess.DEVNULL)
    return [line.split()[-1] for line in out.splitlines() if 'NEEDED' in line]


def image_names(img):
    names = set()
    for block in img.dir_blocks(img.root):
        start = block * img.block_size
        for slot in range(img.block_size // 32):
            entry = start + slot * 32
            inode = int.from_bytes(img.data[entry:entry + 4], 'little')
            if inode:
                length = img.data[entry + 4]
                names.add(img.data[entry + 5:entry + 5 + length].decode())
    return names


def main():
    image = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_IMAGE
    img = Image(image)
    have = image_names(img)

    # phoc must load /sfs libraries from its patched interpreter.
    patched = '/tmp/opencode/phoc.patched'
    data = open(PHOC, 'rb').read()
    open(patched, 'wb').write(data)
    subprocess.run(['patchelf', '--set-interpreter', '/sfs/ld-linux-aarch64.so.1',
                    '--set-rpath', '/sfs', patched], check=True)

    libs = {}
    for name in os.listdir(PHOC_LIBS):
        path = os.path.join(PHOC_LIBS, name)
        if os.path.isfile(path):
            libs.setdefault(name, path)

    written = {}
    queue = needed(patched)
    seen = set()
    missing = []
    while queue:
        soname = queue.pop()
        if soname in seen:
            continue
        seen.add(soname)
        if soname in have or soname in written:
            continue
        path = libs.get(soname)
        if path is None:
            missing.append(soname)
            continue
        payload = open(path, 'rb').read()
        written[soname] = payload
        for name in needed(path):
            queue.append(name)

    if missing:
        print('MISSING', ', '.join(sorted(missing)))
        return 1

    files = {'phoc': open(patched, 'rb').read()}
    files.update(written)
    files['phoc.ini'] = PHOC_INI
    files['phoc-serve'] = open(SERVE, 'rb').read()

    for name, payload in files.items():
        if len(name.encode()) > 27:
            print('name too long for SFS:', name)
            return 1
        img.write_file_at(name, payload)
    img.save()
    print('wrote', len(files), 'files,',
          sum(len(v) for v in files.values()) // (1024 * 1024), 'MiB')
    print('libs:', ', '.join(sorted(written)))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
