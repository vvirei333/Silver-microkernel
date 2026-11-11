#!/usr/bin/env python3
"""Install the aarch64 Mesa EGL/GLES stack into an SFS image.

The guest render node (`/dev/dri/renderD128`) speaks the virtgpu DRM API, so
Mesa's `virtio_gpu` gallium driver works unchanged. This script resolves the
shared-library closure of the EGL/GLES seeds from a staged Arch ARM root,
renames anything that does not fit the SFS 27-byte name limit, patches the
soname references, and writes the files into the image.

Only the surfaceless EGL platform is needed today, but the closure includes
the X11/Wayland platform libraries because they are `DT_NEEDED` of
`libEGL_mesa`.

Usage: tools/install_mesa.py [--root DIR] [--image IMG]
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'scripts'))
from sfs_inject import Image  # noqa: E402

NAME_LIMIT = 27
SEEDS = [
    'libEGL.so.1',
    'libGLESv2.so.2',
    'libEGL_mesa.so.0.0.0',
    'libgbm.so.1.0.0',
    'dri/virtio_gpu_dri.so',
    'gbm/dri_gbm.so',
]
# Mesa 25 (Debian) keeps the real drivers in `mesa-libgallium`; Mesa 26
# (Arch) ships the same file under a versioned name.
EXTRA_SEEDS = ['libgallium-25.0.7-2+deb13u1.so']
# Renames for names that cannot fit. Keep the new name short but still
# recognisable; the soname and every DT_NEEDED entry are patched to match.
SHORTEN = {
    'libgallium-26.2.6-arch1.1.so': 'libgallium-26.2.6.so',
    'libgallium-25.0.7-2+deb13u1.so': 'libgallium-25.0.7.so',
}


def needed(path):
    out = subprocess.check_output(
        ['aarch64-linux-gnu-objdump', '-p', path],
        text=True, stderr=subprocess.DEVNULL)
    return [line.split()[-1] for line in out.splitlines() if 'NEEDED' in line]


def soname(path):
    out = subprocess.check_output(
        ['readelf', '-d', path], text=True, stderr=subprocess.DEVNULL)
    for line in out.splitlines():
        if '(SONAME)' in line:
            return line.split('[')[-1].rstrip(']')
    return os.path.basename(path)


def find(lib, name):
    for directory in (lib, os.path.join(lib, 'dri'), os.path.join(lib, 'gbm'),
                      os.path.join(os.path.dirname(lib), 'lib')):
        candidate = os.path.join(directory, name)
        if os.path.exists(candidate):
            return os.path.realpath(candidate)
    return None


def image_names(img):
    names = set()
    for block in img.dir_blocks(img.root):
        start = block * img.block_size
        for slot in range(img.block_size // 32):
            entry = start + slot * 32
            inode = int.from_bytes(img.data[entry:entry + 4], 'little')
            if inode:
                names.add(img.data[entry + 5:entry + 5 + img.data[entry + 4]]
                          .decode('utf-8', 'replace'))
    return names


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', default=os.path.expanduser('~/phosh-state/gtk4-root'))
    parser.add_argument('--image', default=os.path.expanduser('~/phosh-state/disk_phosh.img'))
    parser.add_argument('--force', action='store_true',
                        help='overwrite files that are already in the image')
    args = parser.parse_args()

    candidates = [
        os.path.join(args.root, 'usr', 'lib', 'aarch64-linux-gnu'),
        os.path.join(args.root, 'usr', 'lib'),
        os.path.join(args.root, 'usr', 'lib64'),
    ]
    lib = next((path for path in candidates
                if os.path.isdir(os.path.join(path, 'dri'))), candidates[1])
    queue = []
    for seed in SEEDS + EXTRA_SEEDS:
        path = os.path.realpath(os.path.join(lib, seed))
        if os.path.exists(path):
            queue.append(path)
    closure = {}
    while queue:
        path = queue.pop()
        name = soname(path)
        if name in closure:
            continue
        closure[name] = path
        for dependency in needed(path):
            if dependency.startswith(('libc.so', 'ld-linux')):
                continue
            found = find(lib, dependency)
            if found and soname(found) not in closure:
                queue.append(found)

    # Renames apply to the closure names.
    renames = {old: new for old, new in SHORTEN.items() if old in closure}

    img = Image(args.image)
    have = image_names(img)
    work = tempfile.mkdtemp(prefix='mesa-')
    written = 0
    total = 0
    try:
        for name, path in sorted(closure.items()):
            target = renames.get(name, name)
            if target in have and not args.force:
                print('skip (present)', target)
                continue
            if len(target) > NAME_LIMIT:
                print('error: %s does not fit the SFS name limit' % target,
                      file=sys.stderr)
                return 1
            copy = os.path.join(work, target)
            shutil.copyfile(path, copy)
            if name in renames:
                subprocess.run(['patchelf', '--set-soname', target, copy],
                               check=True)
            for old, new in renames.items():
                subprocess.run(['patchelf', '--replace-needed', old, new, copy],
                               check=False)
            data = open(copy, 'rb').read()
            img.write_file_at(target, data)
            written += 1
            total += len(data)
            print('wrote %-32s %8d bytes' % (target, len(data)))

        # glvnd dlopens this from libEGL.so.1, which has no RUNPATH of its
        # own, so an absolute path is the only reliable form.
        vendor = (b'{\n    "file_format_version" : "1.0.0",\n'
                  b'    "ICD" : {\n        "library_path" : "/sfs/libEGL_mesa.so.0"\n'
                  b'    }\n}\n')
        img.write_file_at('egl_vendor.json', vendor)
        print('wrote egl_vendor.json')
    finally:
        shutil.rmtree(work, ignore_errors=True)

        # LIBGL_DRIVERS_PATH looks up the driver by the *driver* name, and
        # the staged tree only has the libdril symlink, so ship a copy.
        driver = find(lib, 'virtio_gpu_dri.so')
        if driver is not None and ('virtio_gpu_dri.so' not in have or args.force):
            data = open(driver, 'rb').read()
            img.write_file_at('virtio_gpu_dri.so', data)
            written += 1
            total += len(data)
            print('wrote %-32s %8d bytes' % ('virtio_gpu_dri.so', len(data)))

    img.save()
    print('installed %d files, %.1f MiB' % (written, total / 1048576))
    return 0


if __name__ == '__main__':
    sys.exit(main())
