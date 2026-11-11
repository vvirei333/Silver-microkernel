#!/usr/bin/env python3
"""Generates the WerewolfOS wallpaper and a small symbolic icon theme.

Everything here is drawn from scratch (no third-party artwork): a night-sky
wallpaper and monochrome glyphs for the names Phosh asks for. The icons go into
share/icons/hicolor so GTK finds them through its always-present fallback theme.
"""
import math
import os

from PIL import Image, ImageDraw

OUT = os.environ.get("THEME_OUT", "/tmp/alarm/theme")
SCALE = 4
SIZE = 48


def canvas():
    return Image.new("RGBA", (SIZE * SCALE, SIZE * SCALE), (0, 0, 0, 0))


def save(image, path):
    image = image.resize((SIZE, SIZE), Image.LANCZOS)
    full = os.path.join(OUT, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    image.save(full)


def stroke(draw, points, width, color=(255, 255, 255, 255)):
    draw.line(points, fill=color, width=width, joint="curve")


def rounded(draw, box, radius, width, color=(255, 255, 255, 255)):
    draw.rounded_rectangle(box, radius=radius, outline=color, width=width)


# ---------------------------------------------------------------- wallpaper
def wallpaper(path):
    width, height = 1024, 768
    image = Image.new("RGB", (width, height))
    draw = ImageDraw.Draw(image)
    top = (10, 14, 28)
    bottom = (28, 36, 58)
    for y in range(height):
        t = y / (height - 1)
        draw.line(
            [(0, y), (width, y)],
            fill=tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(3)),
        )
    # stars
    for index in range(90):
        x = (index * 97 + 31) % width
        y = (index * 53 + 17) % (height // 2)
        radius = 1 if index % 5 else 2
        shade = 140 + (index * 13) % 100
        draw.ellipse([x - radius, y - radius, x + radius, y + radius],
                     fill=(shade, shade, min(255, shade + 20)))
    # moon with a soft halo
    cx, cy, r = 780, 190, 96
    for step in range(14, 0, -1):
        alpha = 10 + (14 - step) * 3
        halo = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        ImageDraw.Draw(halo).ellipse(
            [cx - r - step * 9, cy - r - step * 9, cx + r + step * 9, cy + r + step * 9],
            fill=(180, 200, 255, alpha),
        )
        image = Image.alpha_composite(image.convert("RGBA"), halo).convert("RGB")
    draw = ImageDraw.Draw(image)
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(232, 236, 248))
    draw.ellipse([cx - r + 30, cy - r + 18, cx + r - 34, cy + r - 10],
                 fill=(210, 218, 238))
    # a simple horizon of pine silhouettes
    base = height - 70
    for index in range(18):
        x = index * 62 - 20
        h = 60 + (index * 37) % 70
        draw.polygon(
            [(x, base), (x + 26, base - h), (x + 52, base)],
            fill=(8, 12, 22),
        )
    full = os.path.join(OUT, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    image.save(full)


# -------------------------------------------------------------------- icons
def icon_cellular(level, disabled=False):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    if disabled:
        rounded(draw, (8 * SCALE, 8 * SCALE, 40 * SCALE, 40 * SCALE), 8 * SCALE,
                3 * SCALE, (255, 255, 255, 120))
        stroke(draw, [(14 * SCALE, 14 * SCALE), (34 * SCALE, 34 * SCALE)], 3 * SCALE)
        return image
    for bar in range(4):
        x = (10 + bar * 8) * SCALE
        height = (8 + bar * 6) * SCALE
        top = (36 * SCALE) - height
        box = (x, top, x + 5 * SCALE, 36 * SCALE)
        if bar < level:
            draw.rounded_rectangle(box, radius=1 * SCALE, fill=color)
        else:
            rounded(draw, box, 1 * SCALE, 1 * SCALE, (255, 255, 255, 90))
    return image


def icon_wifi(level, disabled=False):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    cx, cy = 24 * SCALE, 34 * SCALE
    if disabled:
        rounded(draw, (8 * SCALE, 8 * SCALE, 40 * SCALE, 40 * SCALE), 8 * SCALE,
                3 * SCALE, (255, 255, 255, 120))
        stroke(draw, [(14 * SCALE, 14 * SCALE), (34 * SCALE, 34 * SCALE)], 3 * SCALE)
        return image
    for ring in range(3):
        radius = (8 + ring * 7) * SCALE
        start = 205
        end = 335
        if ring >= 3 - level:
            draw.arc([cx - radius, cy - radius, cx + radius, cy + radius],
                     start=start, end=end, fill=color, width=3 * SCALE)
    draw.ellipse([cx - 3 * SCALE, cy - 3 * SCALE, cx + 3 * SCALE, cy + 3 * SCALE],
                 fill=color)
    return image


def icon_bluetooth(active=True):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255) if active else (255, 255, 255, 90)
    points = [(24, 8), (34, 18), (14, 34), (34, 34), (14, 18), (24, 8)]
    stroke(draw, [(x * SCALE, y * SCALE) for x, y in points], 3 * SCALE, color)
    return image


def icon_brightness():
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    cx = cy = 24 * SCALE
    draw.ellipse([cx - 7 * SCALE, cy - 7 * SCALE, cx + 7 * SCALE, cy + 7 * SCALE],
                 outline=color, width=3 * SCALE)
    for index in range(8):
        angle = index * math.pi / 4
        inner = 11 * SCALE
        outer = 16 * SCALE
        stroke(draw, [
            (cx + math.cos(angle) * inner, cy + math.sin(angle) * inner),
            (cx + math.cos(angle) * outer, cy + math.sin(angle) * outer),
        ], 3 * SCALE, color)
    return image


def icon_volume(level):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    draw.polygon([(8 * SCALE, 20 * SCALE), (16 * SCALE, 20 * SCALE),
                  (24 * SCALE, 12 * SCALE), (24 * SCALE, 36 * SCALE),
                  (16 * SCALE, 28 * SCALE), (8 * SCALE, 28 * SCALE)], fill=color)
    for ring in range(level):
        radius = (7 + ring * 5) * SCALE
        draw.arc([24 * SCALE - radius, 24 * SCALE - radius,
                  24 * SCALE + radius, 24 * SCALE + radius],
                 start=-55, end=55, fill=color, width=2 * SCALE)
    if level == 0:
        stroke(draw, [(28 * SCALE, 18 * SCALE), (38 * SCALE, 30 * SCALE)], 2 * SCALE)
        stroke(draw, [(38 * SCALE, 18 * SCALE), (28 * SCALE, 30 * SCALE)], 2 * SCALE)
    return image


def icon_battery(level, charging=False):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    rounded(draw, (6 * SCALE, 14 * SCALE, 38 * SCALE, 34 * SCALE), 4 * SCALE, 2 * SCALE)
    draw.rounded_rectangle((38 * SCALE, 20 * SCALE, 42 * SCALE, 28 * SCALE),
                           radius=1 * SCALE, fill=color)
    fill_width = int((30 * SCALE) * max(0, min(100, level)) / 100.0)
    if fill_width > 0:
        draw.rounded_rectangle((9 * SCALE, 17 * SCALE, 9 * SCALE + fill_width, 31 * SCALE),
                               radius=2 * SCALE, fill=color)
    if charging:
        draw.polygon([(24 * SCALE, 12 * SCALE), (16 * SCALE, 26 * SCALE),
                      (23 * SCALE, 26 * SCALE), (20 * SCALE, 38 * SCALE),
                      (30 * SCALE, 22 * SCALE), (23 * SCALE, 22 * SCALE)], fill=color)
    return image


def icon_arrow(direction):
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    if direction == "next":
        stroke(draw, [(18 * SCALE, 14 * SCALE), (30 * SCALE, 24 * SCALE),
                      (18 * SCALE, 34 * SCALE)], 4 * SCALE, color)
    elif direction == "previous":
        stroke(draw, [(30 * SCALE, 14 * SCALE), (18 * SCALE, 24 * SCALE),
                      (30 * SCALE, 34 * SCALE)], 4 * SCALE, color)
    else:
        stroke(draw, [(14 * SCALE, 18 * SCALE), (24 * SCALE, 30 * SCALE),
                      (34 * SCALE, 18 * SCALE)], 4 * SCALE, color)
    return image


def icon_lock():
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    draw.rounded_rectangle((12 * SCALE, 22 * SCALE, 36 * SCALE, 40 * SCALE),
                           radius=4 * SCALE, outline=color, width=3 * SCALE)
    draw.arc([15 * SCALE, 8 * SCALE, 33 * SCALE, 30 * SCALE],
             start=180, end=360, fill=color, width=3 * SCALE)
    draw.ellipse([22 * SCALE, 29 * SCALE, 26 * SCALE, 33 * SCALE], fill=color)
    return image


def icon_gear():
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    cx = cy = 24 * SCALE
    draw.ellipse([cx - 8 * SCALE, cy - 8 * SCALE, cx + 8 * SCALE, cy + 8 * SCALE],
                 outline=color, width=3 * SCALE)
    for index in range(8):
        angle = index * math.pi / 4
        stroke(draw, [
            (cx + math.cos(angle) * 11 * SCALE, cy + math.sin(angle) * 11 * SCALE),
            (cx + math.cos(angle) * 16 * SCALE, cy + math.sin(angle) * 16 * SCALE),
        ], 4 * SCALE, color)
    return image


def icon_refresh():
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 255)
    draw.arc([10 * SCALE, 10 * SCALE, 38 * SCALE, 38 * SCALE],
             start=40, end=320, fill=color, width=4 * SCALE)
    draw.polygon([(33 * SCALE, 8 * SCALE), (40 * SCALE, 18 * SCALE),
                  (29 * SCALE, 17 * SCALE)], fill=color)
    return image


def icon_missing():
    image = canvas()
    draw = ImageDraw.Draw(image)
    color = (255, 255, 255, 140)
    rounded(draw, (9 * SCALE, 9 * SCALE, 39 * SCALE, 39 * SCALE), 7 * SCALE,
            2 * SCALE, color)
    draw.arc([18 * SCALE, 14 * SCALE, 30 * SCALE, 26 * SCALE], start=200, end=20,
             fill=color, width=2 * SCALE)
    draw.ellipse([22 * SCALE, 30 * SCALE, 26 * SCALE, 34 * SCALE], fill=color)
    return image


def build_icons():
    base = "share/icons/hicolor/48x48"
    status = base + "/status"
    actions = base + "/actions"
    apps = base + "/apps"

    for level, name in ((0, "none"), (1, "weak"), (2, "ok"), (3, "good"), (4, "excellent")):
        save(icon_cellular(level), "%s/network-cellular-signal-%s-symbolic.png" % (status, name))
        save(icon_cellular(level), "%s/network-cellular-no-data-signal-%s-symbolic.png" % (status, name))
        save(icon_wifi(level), "%s/network-wireless-signal-%s-symbolic.png" % (status, name))
    save(icon_cellular(0, True), status + "/network-cellular-disabled-symbolic.png")
    save(icon_cellular(3), status + "/network-cellular-symbolic.png")
    save(icon_cellular(3), status + "/mobile-data-symbolic.png")
    save(icon_cellular(0, True), status + "/mobile-data-disabled-symbolic.png")
    save(icon_wifi(4), status + "/network-wireless-symbolic.png")
    save(icon_wifi(4), status + "/network-wireless-hotspot-symbolic.png")
    save(icon_wifi(0, True), status + "/network-offline-symbolic.png")
    save(icon_wifi(0, True), status + "/network-no-route-symbolic.png")
    save(icon_bluetooth(True), status + "/bluetooth-symbolic.png")
    save(icon_wifi(0, True), status + "/network-wireless-disabled-symbolic.png")
    save(icon_wifi(4), status + "/network-wireless-connected-symbolic.png")
    save(icon_bluetooth(True), status + "/bluetooth-active-symbolic.png")
    save(icon_bluetooth(False), status + "/bluetooth-disabled-symbolic.png")
    save(icon_brightness(), status + "/display-brightness-symbolic.png")
    for level, name in ((0, "muted"), (1, "low"), (2, "medium"), (3, "high")):
        save(icon_volume(level), status + "/audio-volume-%s-symbolic.png" % name)
    save(icon_volume(2), status + "/audio-speakers-symbolic.png")
    save(icon_volume(3), status + "/audio-volume-symbolic.png")
    save(icon_battery(100), status + "/battery-level-100-symbolic.png")
    save(icon_battery(100, True), status + "/battery-level-100-charged-symbolic.png")
    for level in (0, 10, 20, 30, 40, 50, 60, 70, 80, 90):
        save(icon_battery(level), "%s/battery-level-%d-symbolic.png" % (status, level))
    save(icon_battery(50), status + "/battery-missing-symbolic.png")

    save(icon_arrow("next"), actions + "/go-next-symbolic.png")
    save(icon_arrow("previous"), actions + "/go-previous-symbolic.png")
    save(icon_arrow("down"), actions + "/go-down-symbolic.png")
    save(icon_arrow("down"), actions + "/swipe-arrow-symbolic.png")
    save(icon_lock(), actions + "/padlock-symbolic.png")
    save(icon_lock(), actions + "/system-lock-screen-symbolic.png")
    save(icon_gear(), actions + "/settings-symbolic.png")
    save(icon_refresh(), actions + "/view-refresh-symbolic.png")
    save(icon_missing(), actions + "/image-missing-symbolic.png")
    save(icon_missing(), apps + "/app-icon-unknown-symbolic.png")


def index_theme():
    path = os.path.join(OUT, "share/icons/hicolor/index.theme")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as handle:
        handle.write(
            "[Icon Theme]\n"
            "Name=Hicolor\n"
            "Comment=Fallback icon theme\n"
            "Directories=48x48/actions,48x48/status,48x48/apps\n"
            "\n"
            "[48x48/actions]\nSize=48\nContext=Actions\nType=Fixed\n"
            "\n"
            "[48x48/status]\nSize=48\nContext=Status\nType=Fixed\n"
            "\n"
            "[48x48/apps]\nSize=48\nContext=Applications\nType=Fixed\n"
        )


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    wallpaper("share/backgrounds/werewolf.png")
    build_icons()
    index_theme()
    count = sum(len(files) for _, _, files in os.walk(OUT))
    print("generated", count, "files in", OUT)


def make_pack(path):
    """Packs the icon tree into one short-named file.

    The SFS directory entry holds at most 27 bytes of name, and GTK icon names
    such as `network-wireless-signal-excellent-symbolic.png` are longer. The
    compositor unpacks this archive into /tmp at startup instead.
    """
    import struct
    entries = []
    for base, _, files in os.walk(os.path.join(OUT, "share/icons")):
        for name in files:
            full = os.path.join(base, name)
            rel = os.path.relpath(full, OUT)
            with open(full, "rb") as handle:
                entries.append((rel, handle.read()))
    entries.sort()
    with open(path, "wb") as handle:
        handle.write(b"WPK1")
        handle.write(struct.pack("<I", len(entries)))
        for rel, data in entries:
            encoded = rel.encode()
            handle.write(struct.pack("<H", len(encoded)))
            handle.write(encoded)
            handle.write(struct.pack("<I", len(data)))
            handle.write(data)
    return len(entries)
