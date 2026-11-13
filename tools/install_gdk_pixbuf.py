#!/usr/bin/env python3
"""Install a gdk-pixbuf build with built-in PNG/JPEG into an SFS image.

The Arch aarch64 gdk-pixbuf (2.44) delegates every decode to glycin. In this
guest glycin's gdk-pixbuf path either reports "Failed to get the filename" or
times out after 60 s, which makes GTK abort while loading `image-missing.png`
and leaves Phosh without a screen.

Debian 13 still ships gdk-pixbuf 2.42.12 with the PNG and JPEG loaders compiled
into `libgdk_pixbuf-2.0.so.0`. The two GLib-based libraries share the same
`libgdk_pixbuf-2.0.so.0` soname and every `gdk_pixbuf_*` symbol imported by the
guest's GTK4/phosh is exported by the Debian build, so replacing the library is
enough (no loader modules or `loaders.cache` are involved).

The Debian build links `libjpeg.so.62`, which Arch does not ship (it has
`libjpeg.so.8`), so the matching `libjpeg62-turbo` is installed as well.

Usage: tools/install_gdk_pixbuf.py [--image IMG] [--cache DIR]
"""
import argparse
import io
import os
import shutil
import sys
import tarfile
import tempfile
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'scripts'))
from sfs_inject import Image  # noqa: E402

DEBIAN = 'http://deb.debian.org/debian/pool/main'
PACKAGES = [
    (f'{DEBIAN}/g/gdk-pixbuf/libgdk-pixbuf-2.0-0_2.42.12+dfsg-4+deb13u1_arm64.deb',
     'libgdk_pixbuf-2.0.so.0.4200.12', 'libgdk_pixbuf-2.0.so.0'),
    (f'{DEBIAN}/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_arm64.deb',
     'libjpeg.so.62.3.0', 'libjpeg.so.62'),
]


def download(url, cache):
    name = url.rsplit('/', 1)[-1]
    if cache is not None:
        os.makedirs(cache, exist_ok=True)
        path = os.path.join(cache, name)
        if os.path.exists(path) and os.path.getsize(path) > 0:
            return path
    else:
        path = os.path.join(tempfile.mkdtemp(prefix='gdkpixbuf-'), name)
    urllib.request.urlretrieve(url, path)
    return path


def ar_members(path):
    """Yields (name, bytes) from a Debian `ar` archive."""
    with open(path, 'rb') as handle:
        if handle.read(8) != b'!<arch>\n':
            raise SystemExit(f'{path}: not an ar archive')
        while True:
            header = handle.read(60)
            if len(header) < 60:
                return
            name = header[:16].decode('ascii', 'replace').strip().rstrip('/')
            size = int(header[48:58].decode('ascii').strip())
            data = handle.read(size)
            if size & 1:
                handle.read(1)
            yield name, data


def extract_member(deb, wanted, destination):
    for name, data in ar_members(deb):
        if name == 'data.tar.xz':
            with tarfile.open(fileobj=io.BytesIO(data), mode='r:xz') as archive:
                for member in archive.getmembers():
                    base = member.name.rsplit('/', 1)[-1]
                    if base == wanted and member.isfile():
                        source = archive.extractfile(member)
                        with open(destination, 'wb') as out:
                            shutil.copyfileobj(source, out)
                        return True
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--image', default=os.path.expanduser('~/phosh-state/disk_phosh.img'))
    parser.add_argument('--cache', default=None,
                        help='keep the downloaded .debs in this directory')
    options = parser.parse_args()

    image = Image(options.image)
    for url, member, target in PACKAGES:
        deb = download(url, options.cache)
        with tempfile.NamedTemporaryFile(prefix='sfs-', delete=False) as staging:
            staged = staging.name
        try:
            if not extract_member(deb, member, staged):
                raise SystemExit(f'{member} not found in {deb}')
            data = open(staged, 'rb').read()
            image.write_file_at(target, data)
            print(f'wrote {target} ({len(data)} bytes)')
        finally:
            os.unlink(staged)
    image.save()


if __name__ == '__main__':
    main()
