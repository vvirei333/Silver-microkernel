#!/usr/bin/env python3
"""Render the fastfetch screen from a real serial capture into a PNG.

The capture is the raw byte stream produced by the guest's PL011 console
(`\x1b[..m` colour codes, `\r\n` line ends).  Keeping the capture in the
repository and rendering it with this script makes the README screenshot
reproducible: the text, colours, version numbers and resource counters all
come from an actual boot, not from a mock-up.  Only the window chrome
(rounded frame, title bar, traffic lights) is drawn by this script.

    tools/make_fastfetch_asset.py assets/fastfetch-0.1.2.ansi \
        assets/silver-fastfetch-0.1.2.png

Capture recipe (host):

    ./build_and_qemu.sh --no-viewer      # or the /tmp harness
    # at `howl>` run `fastfetch`, then save the serial socket bytes
    # between the echoed command and the next prompt.
"""
import argparse
import re
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# Standard 8 + bright 8 colour palette (the guest's VGA-compatible table).
ANSI = {
    30: (0x00, 0x00, 0x00), 31: (0xAA, 0x00, 0x00), 32: (0x00, 0xAA, 0x00),
    33: (0xAA, 0x55, 0x00), 34: (0x00, 0x00, 0xAA), 35: (0xAA, 0x00, 0xAA),
    36: (0x00, 0xAA, 0xAA), 37: (0xAA, 0xAA, 0xAA),
    90: (0x55, 0x55, 0x55), 91: (0xFF, 0x55, 0x55), 92: (0x55, 0xFF, 0x55),
    93: (0xFF, 0xFF, 0x55), 94: (0x55, 0x55, 0xFF), 95: (0xFF, 0x55, 0xFF),
    96: (0x55, 0xFF, 0xFF), 97: (0xFF, 0xFF, 0xFF),
    40: (0x00, 0x00, 0x00), 41: (0xAA, 0x00, 0x00), 42: (0x00, 0xAA, 0x00),
    43: (0xAA, 0x55, 0x00), 44: (0x00, 0x00, 0xAA), 45: (0xAA, 0x00, 0xAA),
    46: (0x00, 0xAA, 0xAA), 47: (0xAA, 0xAA, 0xAA),
    100: (0x55, 0x55, 0x55), 101: (0xFF, 0x55, 0x55), 102: (0x55, 0xFF, 0x55),
    103: (0xFF, 0xFF, 0x55), 104: (0x55, 0x55, 0xFF), 105: (0xFF, 0x55, 0xFF),
    106: (0x55, 0xFF, 0xFF), 107: (0xFF, 0xFF, 0xFF),
}

DEFAULT_FG = (0xD8, 0xDC, 0xE6)
DEFAULT_BG = (0x05, 0x07, 0x0D)

FONT_CANDIDATES = [
    "/usr/share/fonts/TTF/MesloLGLDZNerdFontMono-Regular.ttf",
    "/usr/share/fonts/TTF/MesloLGSDZNerdFontMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
]
SANS_CANDIDATES = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
]


def first_existing(paths):
    for path in paths:
        if Path(path).exists():
            return path
    raise SystemExit("no usable font found; install a monospace TTF")


def parse_ansi(data: bytes):
    """Return a list of lines, each a list of (text, fg, bg) segments."""
    text = data.decode("utf-8", "replace")
    lines = []
    line = []
    fg, bg, bold = DEFAULT_FG, None, False

    token = re.compile(r"\x1b\[([0-9;]*)m|\r\n|\r|\n")

    pos = 0
    for match in token.finditer(text):
        if match.start() > pos:
            line.append((text[pos:match.start()], fg, bg))
        pos = match.end()
        if match.group(0) in ("\r\n", "\r", "\n"):
            lines.append(line)
            line = []
            continue
        params = [int(p) for p in match.group(1).split(";") if p != ""] or [0]
        index = 0
        while index < len(params):
            code = params[index]
            if code == 0:
                fg, bg, bold = DEFAULT_FG, None, False
            elif code == 1:
                bold = True
            elif code in (30, 31, 32, 33, 34, 35, 36, 37, 90, 91, 92, 93,
                          94, 95, 96, 97):
                fg = ANSI[code]
                if code in (90, 91, 92, 93, 94, 95, 96, 97):
                    bold = False
            elif code in (40, 41, 42, 43, 44, 45, 46, 47, 100, 101, 102,
                          103, 104, 105, 106, 107):
                bg = ANSI[code]
            elif code == 39:
                fg = DEFAULT_FG
            elif code == 49:
                bg = None
            index += 1
    if pos < len(text):
        line.append((text[pos:], fg, bg))
    if line:
        lines.append(line)
    return lines


def trim(lines):
    """Drop the echoed command prefix noise and stop at the next prompt."""
    cleaned = []
    started = False
    for line in lines:
        plain = "".join(t for t, _, _ in line).replace("\x00", "")
        if not started:
            if "fastfetch" in plain and plain.strip() == "fastfetch":
                started = True
                # Re-render the prompt prefix in the guest's cyan (the serial
                # capture starts after the prompt was already printed).
                cleaned.append([("howl> ", (0x00, 0xAA, 0xAA), None),
                                ("fastfetch", DEFAULT_FG, None)])
            continue
        if "howl>" in plain:
            cleaned.append([("howl> ", (0x00, 0xAA, 0xAA), None)])
            break
        cleaned.append(line)
    return cleaned


def fit_font(cols, rows, avail_w, avail_h):
    path = first_existing(FONT_CANDIDATES)
    best = None
    for size in range(8, 48):
        font = ImageFont.truetype(path, size)
        cell = font.getlength("M")
        line_h = round(size * 1.42)
        if cols * cell <= avail_w and rows * line_h <= avail_h:
            best = (font, cell, line_h, size)
    if best is None:
        size = 8
        font = ImageFont.truetype(path, size)
        best = (font, font.getlength("M"), round(size * 1.42), size)
    return best


def render(lines, out_path, width=1440, height=1440):
    title_h = 56
    margin = 64
    avail_w = width - 2 * margin
    avail_h = height - title_h - 2 * margin
    cols = max(len("".join(t for t, _, _ in line)) for line in lines)
    rows = len(lines)
    font, cell, line_h, size = fit_font(cols, rows, avail_w, avail_h)

    image = Image.new("RGB", (width, height), (0x0B, 0x0F, 0x17))
    draw = ImageDraw.Draw(image)

    # Window frame with a title bar.
    draw.rounded_rectangle((24, 24, width - 24, height - 24), radius=18,
                           fill=DEFAULT_BG, outline=(0x22, 0x28, 0x36),
                           width=2)
    draw.rounded_rectangle((24, 24, width - 24, 24 + title_h), radius=18,
                           fill=(0x1B, 0x20, 0x2B))
    draw.rectangle((24, 24 + title_h - 10, width - 24, 24 + title_h),
                   fill=(0x1B, 0x20, 0x2B))
    for index, colour in enumerate(((0xFF, 0x5F, 0x57),
                                    (0xFE, 0xBC, 0x2E),
                                    (0x28, 0xC8, 0x40))):
        cx = 58 + index * 26
        draw.ellipse((cx - 7, 24 + title_h // 2 - 7,
                      cx + 7, 24 + title_h // 2 + 7), fill=colour)
    sans_path = first_existing(SANS_CANDIDATES)
    title_font = ImageFont.truetype(sans_path, 22)
    title = "WerewolfOS  |  QEMU / ARM64  |  0.1.2"
    tw = draw.textlength(title, font=title_font)
    draw.text(((width - tw) / 2, 24 + (title_h - 22) / 2 - 3), title,
              font=title_font, fill=(0xC8, 0xCC, 0xD4))

    # Terminal text, centred horizontally.
    block_w = cols * cell
    x0 = (width - block_w) / 2
    y = 24 + title_h + (height - 24 - (24 + title_h) - rows * line_h) / 2 + 8

    for line in lines:
        x = x0
        for text, fg, bg in line:
            if bg is not None:
                draw.rectangle((x, y, x + cell * len(text), y + line_h),
                               fill=bg)
            if text.strip():
                draw.text((x, y + (line_h - size) / 2 - size * 0.12), text,
                          font=font, fill=fg)
            x += cell * len(text)
        y += line_h

    image.save(out_path)
    print("wrote %s (%dx%d, %d cols x %d rows, %.1fpx font)" %
          (out_path, width, height, cols, rows, size))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("capture", help="raw serial bytes with ANSI sequences")
    parser.add_argument("output", help="PNG to write")
    parser.add_argument("--width", type=int, default=1440)
    parser.add_argument("--height", type=int, default=1440)
    args = parser.parse_args()

    data = Path(args.capture).read_bytes()
    lines = trim(parse_ansi(data))
    if not lines:
        sys.exit("capture contains no fastfetch block")
    render(lines, args.output, args.width, args.height)


if __name__ == "__main__":
    main()
