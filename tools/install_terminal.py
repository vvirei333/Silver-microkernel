#!/usr/bin/env python3
"""Install the WerewolfOS terminal into an SFS image.

Installs three things:

  /busybox                 static Debian busybox (the applet set)
  /term                    the GTK3 terminal application
  /share/applications/     a launcher entry so Phosh's app grid lists it

and the 48x48 icon in the hicolor theme.

The terminal binary is a GTK3 program because Phosh (and therefore the image)
is GTK3 + libhandy. Build it with:

    G=~/phosh-state/gtk4-root
    aarch64-linux-gnu-gcc -O2 -o build/term tests/linux/term.c \
        $(pkg-config --cflags gtk+-3.0) -L$G/usr/lib \
        -Wl,-rpath-link,$G/usr/lib -lgtk-3 -lgdk-3 -lpangocairo-1.0 \
        -lpango-1.0 -lharfbuzz -latk-1.0 -lcairo-gobject -lcairo \
        -lgdk_pixbuf-2.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/term

The terminal needs the compositor's `term-serve` wrapper (built from
tests/linux/wayland_real.c with `-DSERVE_ONLY -DSERVE_TERM`), so `start`
it with `linuxstart term`.

Usage: tools/install_terminal.py --image IMG [--term build/term]
                                 [--busybox PATH] [--icon PATH]
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'scripts'))
from sfs_inject import Image  # noqa: E402

DESKTOP = b'''[Desktop Entry]
Type=Application
Name=Terminal
Comment=Run commands on WerewolfOS
Exec=/sfs/term
Icon=utilities-terminal
Terminal=false
Categories=System;Utility;
Keywords=shell;command;busybox;
StartupNotify=false
'''

DEFAULT_ICON = os.path.expanduser(
    '~/phosh-state/gtk4-root/usr/share/icons/AdwaitaLegacy/48x48/legacy/'
    'utilities-terminal.png')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--image', default=os.path.expanduser('~/phosh-state/disk_phosh.img'))
    parser.add_argument('--term', default='build/term')
    parser.add_argument('--busybox', default=None,
                        help='static busybox binary (default: fetch from Debian)')
    parser.add_argument('--icon', default=DEFAULT_ICON)
    options = parser.parse_args()

    image = Image(options.image)
    term = open(options.term, 'rb').read()
    image.write_file_at('term', term)
    print(f'wrote term ({len(term)} bytes)')

    if options.busybox is not None:
        busybox = open(options.busybox, 'rb').read()
    else:
        import urllib.request
        import tempfile
        url = ('http://deb.debian.org/debian/pool/main/b/busybox/'
               'busybox-static_1.38.0-3+b2_arm64.deb')
        print(f'fetching {url}')
        with tempfile.NamedTemporaryFile(prefix='busybox-', delete=False) as handle:
            staged = handle.name
        try:
            urllib.request.urlretrieve(url, staged)
            busybox = extract_busybox(staged)
        finally:
            os.unlink(staged)
    image.write_file_at('busybox', busybox)
    print(f'wrote busybox ({len(busybox)} bytes)')

    image.write_file_at('share/applications/werewolf-terminal.desktop', DESKTOP)
    print('wrote share/applications/werewolf-terminal.desktop')

    if os.path.exists(options.icon):
        icon = open(options.icon, 'rb').read()
        image.write_file_at('share/icons/hicolor/48x48/apps/utilities-terminal.png',
                            icon)
        print(f'wrote icon ({len(icon)} bytes)')
    else:
        print(f'note: icon {options.icon} missing; the tile uses the theme fallback')

    image.save()


def extract_busybox(deb):
    """Returns the ``/bin/busybox`` member of a Debian .deb archive."""
    import io
    import tarfile

    with open(deb, 'rb') as handle:
        if handle.read(8) != b'!<arch>\n':
            raise SystemExit(f'{deb}: not an ar archive')
        while True:
            header = handle.read(60)
            if len(header) < 60:
                break
            name = header[:16].decode('ascii', 'replace').strip().rstrip('/')
            size = int(header[48:58].decode('ascii').strip())
            data = handle.read(size)
            if size & 1:
                handle.read(1)
            if name != 'data.tar.xz':
                continue
            with tarfile.open(fileobj=io.BytesIO(data), mode='r:xz') as archive:
                for member in archive.getmembers():
                    if member.name.endswith('bin/busybox') and member.isfile():
                        return archive.extractfile(member).read()
    raise SystemExit(f'{deb}: no bin/busybox')


if __name__ == '__main__':
    main()
