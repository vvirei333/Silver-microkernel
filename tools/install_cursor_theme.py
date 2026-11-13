#!/usr/bin/env python3
"""Write a minimal Xcursor theme into an SFS image.

GTK asks the compositor for a cursor by name and loads the image from an
Xcursor theme. The image had none, so every pointer enter produced
`Unable to load <name> from the cursor theme`, and each attempt re-scanned
every icon directory before failing; that flood is what filled the serial
console and slowed application startup.

The theme here is generated, not copied: a handful of 24x24 shapes (arrow,
I-beam, crosshair, ring) written as Xcursor files under
`/share/icons/default/cursors`. Every name GTK3 asks for is present, so the
lookup always succeeds. `index.theme` is included for tools that expect it.

Usage: tools/install_cursor_theme.py --image IMG [--size 24]
"""

from __future__ import annotations

import argparse
import os
import struct
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "scripts"))

from sfs_inject import Image  # noqa: E402

THEME = "default"

# left_ptr, 16x16, as the classic X11 mask (one bit per pixel).
ARROW_ROWS = [
    0x0000, 0x0002, 0x0006, 0x000E, 0x001E, 0x003E, 0x007E, 0x00FE,
    0x01FE, 0x03FE, 0x003E, 0x0036, 0x0062, 0x0060, 0x00C0, 0x00C0,
]

TEXT_ROWS = [
    "....X....", "....X....", "...XXX...", "....X....",
    "....X....", "....X....", "....X....", "....X....",
    "....X....", "....X....", "....X....", "....X....",
    "...XXX...", "....X....", "....X....", "....X....",
]

# Cursor names GTK3 (gdkcursor-wayland.c) and Phosh ask for. Everything that
# is not listed as a special shape is drawn as the arrow.
NAMES = {
    "default": "arrow",
    "left_ptr": "arrow",
    "top_left_arrow": "arrow",
    "pointer": "arrow",
    "dnd-ask": "arrow",
    "dnd-copy": "arrow",
    "dnd-move": "arrow",
    "dnd-none": "arrow",
    "text": "text",
    "vertical-text": "text",
    "xterm": "text",
    "crosshair": "cross",
    "cell": "cross",
    "cross": "cross",
    "move": "cross",
    "all-scroll": "cross",
    "fleur": "cross",
    "no-drop": "ring",
    "not-allowed": "ring",
    "forbidden": "ring",
    "progress": "arrow",
    "wait": "arrow",
    "watch": "arrow",
    "help": "arrow",
    "context-menu": "arrow",
    "alias": "arrow",
    "copy": "arrow",
    "grab": "arrow",
    "grabbing": "arrow",
    "zoom-in": "arrow",
    "zoom-out": "arrow",
    "e-resize": "arrow",
    "w-resize": "arrow",
    "n-resize": "arrow",
    "s-resize": "arrow",
    "ne-resize": "arrow",
    "nw-resize": "arrow",
    "se-resize": "arrow",
    "sw-resize": "arrow",
    "ew-resize": "arrow",
    "ns-resize": "arrow",
    "nesw-resize": "arrow",
    "nwse-resize": "arrow",
    "col-resize": "arrow",
    "row-resize": "arrow",
}
ARROW_HOTSPOT = (1, 1)


def arrow_mask() -> list[list[bool]]:
    mask = [[False] * 16 for _ in range(16)]
    for y, bits in enumerate(ARROW_ROWS):
        for x in range(16):
            if bits & (1 << x):
                mask[y][x] = True
    return mask


def text_mask() -> list[list[bool]]:
    width = len(TEXT_ROWS[0])
    left = (16 - width) // 2
    mask = [[False] * 16 for _ in range(16)]
    for y, row in enumerate(TEXT_ROWS):
        for x, column in enumerate(row):
            if column == "X":
                mask[y][left + x] = True
    return mask


def cross_mask() -> list[list[bool]]:
    mask = [[False] * 16 for _ in range(16)]
    for index in range(16):
        mask[7][index] = True
        mask[index][7] = True
    return mask


def ring_mask() -> list[list[bool]]:
    mask = [[False] * 16 for _ in range(16)]
    for y in range(16):
        for x in range(16):
            distance = (x - 7.5) ** 2 + (y - 7.5) ** 2
            if 20 <= distance <= 48:
                mask[y][x] = True
    return mask


def render(mask: list[list[bool]], size: int) -> list[int]:
    """Scales the 16x16 mask up to `size` and outlines it in white."""
    source = len(mask)
    scaled = [[False] * size for _ in range(size)]
    for y in range(size):
        for x in range(size):
            scaled[y][x] = mask[y * source // size][x * source // size]
    pixels: list[int] = []
    for y in range(size):
        for x in range(size):
            if scaled[y][x]:
                pixels.append(0xFF000000)  # black, opaque
                continue
            outline = False
            for dy in (-1, 0, 1):
                for dx in (-1, 0, 1):
                    nx = x + dx
                    ny = y + dy
                    if 0 <= nx < size and 0 <= ny < size and scaled[ny][nx]:
                        outline = True
            pixels.append(0xFFFFFFFF if outline else 0x00000000)
    return pixels


def xcursor(shapes: dict[str, list[list[bool]]], name: str, size: int) -> bytes:
    mask = shapes[NAMES.get(name, "arrow")]
    pixels = render(mask, size)
    hot_x, hot_y = ARROW_HOTSPOT if NAMES.get(name, "arrow") == "arrow" else (size // 2, size // 2)
    hot_x = hot_x * size // len(mask)
    hot_y = hot_y * size // len(mask)
    image = struct.pack(
        "<IIIIIIIII", 36, 0xFFFD0002, size, 1, size, size, hot_x, hot_y, 0
    ) + struct.pack("<%dI" % len(pixels), *pixels)
    header = struct.pack("<4sIII", b"Xcur", 16, 0x00010000, 1)
    table = struct.pack("<III", 0xFFFD0002, size, 16 + 12)
    return header + table + image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, help="SFS image to modify")
    parser.add_argument("--size", type=int, default=24, help="nominal cursor size")
    parser.add_argument("--theme", default=THEME, help="theme directory name")
    options = parser.parse_args()

    shapes = {
        "arrow": arrow_mask(),
        "text": text_mask(),
        "cross": cross_mask(),
        "ring": ring_mask(),
    }

    image = Image(options.image)
    root = f"share/icons/{options.theme}"
    image.mkdir(f"share/icons")
    image.mkdir(root)
    image.mkdir(f"{root}/cursors")
    index = (
        "[Icon Theme]\n"
        f"Name={options.theme}\n"
        "Comment=WerewolfOS pointer cursors\n"
        "Inherits=hicolor\n"
    )
    image.write_file_at(f"{root}/index.theme", index.encode())
    for name in NAMES:
        image.write_file_at(f"{root}/cursors/{name}",
                            xcursor(shapes, name, options.size))
    image.save()
    print(f"{options.image}: wrote {len(NAMES)} cursors into /{root}/cursors")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
