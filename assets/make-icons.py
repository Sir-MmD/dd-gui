#!/usr/bin/env python3
"""Builds every DD-GUI icon file from one set of parameters.

    python3 assets/make-icons.py

Needs Python 3 (stdlib only), rsvg-convert and ImageMagick 7 (`magick`).

Writes:
    assets/icon.svg              master, 1024 x 1024 (used for 128 px and up)
    assets/icon-small.svg        the 32 px version, drawn on the pixel grid
    assets/png/DD-GUI-<n>.png    16 24 32 48 64 128 256 512 1024
    assets/DD-GUI.ico            16 24 32 48 64 128 256
    assets/DD-GUI.icns           16 to 1024, macOS icon grid
    ui/logo.svg                  in-app mark, hinted for 30 px

The mark: a lowercase "dd" whose bowls are disk platters (hub, clamp ring)
and whose stems are the read/write arms lying over them. The first d is the
source (white), the second the copy (orange). At 64 px and below each size is
drawn on its own pixel grid; the platter detail is dropped as space runs out.
"""
import math
import os
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# Palette
TILE = "#1C1C1F"       # graphite tile
TILE_EDGE = "#2E2E33"  # machined rim, visible on dark desktops at large sizes
WHITE = "#F2F2F3"      # source
ORANGE = "#FF7A1A"     # copy

SQUIRCLE_R = 0.2237    # corner radius / tile size (macOS Big Sur shape)


def num(v):
    s = f"{v:.3f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


# ------------------------------------------------------------------ shapes
def squircle(x, y, w, h, r, smooth=0.6):
    """Rounded rect with continuous-curvature corners (Figma-style smoothing)."""
    p = min((1 + smooth) * r, w / 2, h / 2)
    arc = 90 * (1 - smooth)
    arc_len = math.sin(math.radians(arc / 2)) * r * math.sqrt(2)
    p34 = r * math.tan(math.radians((90 - arc) / 4))
    beta = math.radians(45 * smooth)
    c = p34 * math.cos(beta)
    d = c * math.tan(beta)
    b = (p - arc_len - c - d) / 3
    a = 2 * b
    n = num
    return (
        f"M{n(x + w - p)} {n(y)}"
        f"c{n(a)} 0 {n(a + b)} 0 {n(a + b + c)} {n(d)}"
        f"a{n(r)} {n(r)} 0 0 1 {n(arc_len)} {n(arc_len)}"
        f"c{n(d)} {n(c)} {n(d)} {n(b + c)} {n(d)} {n(a + b + c)}"
        f"L{n(x + w)} {n(y + h - p)}"
        f"c0 {n(a)} 0 {n(a + b)} {n(-d)} {n(a + b + c)}"
        f"a{n(r)} {n(r)} 0 0 1 {n(-arc_len)} {n(arc_len)}"
        f"c{n(-c)} {n(d)} {n(-(b + c))} {n(d)} {n(-(a + b + c))} {n(d)}"
        f"L{n(x + p)} {n(y + h)}"
        f"c{n(-a)} 0 {n(-(a + b))} 0 {n(-(a + b + c))} {n(-d)}"
        f"a{n(r)} {n(r)} 0 0 1 {n(-arc_len)} {n(-arc_len)}"
        f"c{n(-d)} {n(-c)} {n(-d)} {n(-(b + c))} {n(-d)} {n(-(a + b + c))}"
        f"L{n(x)} {n(y + p)}"
        f"c0 {n(-a)} 0 {n(-(a + b))} {n(d)} {n(-(a + b + c))}"
        f"a{n(r)} {n(r)} 0 0 1 {n(arc_len)} {n(-arc_len)}"
        f"c{n(c)} {n(-d)} {n(b + c)} {n(-d)} {n(a + b + c)} {n(-d)}Z"
    )


def rounded(x, y, w, h, r):
    """Plain rounded rect; at icon sizes up to 64 px it matches the squircle."""
    n = num
    return (f"M{n(x + r)} {n(y)}h{n(w - 2 * r)}a{n(r)} {n(r)} 0 0 1 {n(r)} {n(r)}"
            f"v{n(h - 2 * r)}a{n(r)} {n(r)} 0 0 1 {n(-r)} {n(r)}h{n(-(w - 2 * r))}"
            f"a{n(r)} {n(r)} 0 0 1 {n(-r)} {n(-r)}v{n(-(h - 2 * r))}"
            f"a{n(r)} {n(r)} 0 0 1 {n(r)} {n(-r)}Z")


def rect(x, y, w, h):
    return f"M{num(x)} {num(y)}h{num(w)}v{num(h)}h{num(-w)}Z"


def circle(cx, cy, r):
    return (f"M{num(cx - r)} {num(cy)}a{num(r)} {num(r)} 0 1 0 {num(2 * r)} 0"
            f"a{num(r)} {num(r)} 0 1 0 {num(-2 * r)} 0Z")


def disk_left_of(cx, cy, r, xc):
    """The part of a disk left of the line x = xc (cx < xc < cx + r)."""
    dy = math.sqrt(r * r - (xc - cx) ** 2)
    return f"M{num(xc)} {num(cy - dy)}A{num(r)} {num(r)} 0 1 0 {num(xc)} {num(cy + dy)}Z"


def ring_left_of(cx, cy, r_out, r_in, xc):
    """An annulus clipped to x < xc, as one closed band."""
    dx = xc - cx
    if dx >= r_out:
        return circle(cx, cy, r_out) + circle(cx, cy, r_in)
    do = math.sqrt(r_out ** 2 - dx ** 2)
    if dx >= r_in:
        return disk_left_of(cx, cy, r_out, xc) + circle(cx, cy, r_in)
    di = math.sqrt(r_in ** 2 - dx ** 2)
    return (f"M{num(xc)} {num(cy - do)}A{num(r_out)} {num(r_out)} 0 1 0 {num(xc)} {num(cy + do)}"
            f"L{num(xc)} {num(cy + di)}A{num(r_in)} {num(r_in)} 0 1 1 {num(xc)} {num(cy - di)}Z")


def pixel_disk(x, y, n):
    """A disk of diameter n as whole-pixel runs: crisper than anti-aliasing below ~40 px."""
    out = ""
    c = n / 2
    for row in range(n):
        half = math.sqrt(max(c * c - (row + 0.5 - c) ** 2, 0))
        x0, x1 = round(c - half), round(c + half)
        if x1 > x0:
            out += rect(x + x0, y + row, x1 - x0, 1)
    return out


def svg(size, body, title="DD-GUI"):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" '
            f'viewBox="0 0 {size} {size}">\n<title>{title}</title>\n{body}\n</svg>\n')


# ------------------------------------------------------------------ master
# Geometry on a 1000-unit tile.
R = 162        # platter radius
S = 86         # arm width (the arm's right edge sits on the platter's right edge)
L = 66         # space between the two letters
ASC = 182      # ascender: how far the arm rises above the platter
CUT = 15       # gap between arm and platter
HUB = 32       # hub hole radius
TRACK = 0.46   # hub-clamp ring radius, as a fraction of R
TRACK_W = 8    # ring width
PIVOT = 14     # pivot hole radius, near the top of the arm
DY = -22       # optical lift of the whole mark
EDGE = 6       # tile rim width


def master_art():
    """The mark and tile in 1000-unit tile space."""
    parts = [
        f'<path fill="{TILE_EDGE}" d="{squircle(0, 0, 1000, 1000, 1000 * SQUIRCLE_R)}"/>',
        f'<path fill="{TILE}" d="{squircle(EDGE, EDGE, 1000 - 2 * EDGE, 1000 - 2 * EDGE, 1000 * SQUIRCLE_R - EDGE)}"/>',
    ]
    H = 2 * R + ASC
    x0 = (1000 - (4 * R + L)) / 2
    top = (1000 - H) / 2 + DY
    for i, color in enumerate((WHITE, ORANGE)):
        ox = x0 + i * (2 * R + L)
        cx, cy = ox + R, top + H - R
        sx = ox + 2 * R - S
        xc = sx - CUT
        rt = R * TRACK
        d = (disk_left_of(cx, cy, R, xc)
             + ring_left_of(cx, cy, rt + TRACK_W / 2, rt - TRACK_W / 2, xc)
             + circle(cx, cy, HUB)
             + rect(sx, top, S, H)
             + circle(sx + S / 2, top + S / 2, PIVOT))
        parts.append(f'<path fill="{color}" fill-rule="evenodd" d="{d}"/>')
    return "\n".join(parts)


def master_svg(canvas=1024, inset=64):
    """inset 64/1024: a 896 px tile, for Windows and Linux (and the master file).
    inset 100/1024: the 824 px tile of Apple's macOS icon grid."""
    k = (canvas - 2 * inset) / 1000
    return svg(canvas, f'<g transform="translate({num(inset)} {num(inset)}) scale({num(k)})">\n'
                       f'{master_art()}\n</g>')


# ------------------------------------------------------------------ hinted sizes
# Every edge on a whole pixel. bowl/stem/gap/asc/hub in px; left/top place the
# glyph; inset/radius the tile. pixel=True draws the platters as pixel runs.
HINTED = {
    16: dict(bowl=6, stem=2, gap=2, asc=3, hub=2, left=1, top=3, inset=0, radius=3.5, pixel=True),
    24: dict(bowl=8, stem=3, gap=2, asc=4, hub=2, left=3, top=6, inset=0, radius=5.25, pixel=True),
    32: dict(bowl=10, stem=3, gap=2, asc=6, hub=2, left=5, top=8, inset=0, radius=7, pixel=True),
    48: dict(bowl=14, stem=4, gap=4, asc=8, hub=2, left=8, top=12, inset=3, radius=9.5, cut=1),
    64: dict(bowl=18, stem=5, gap=4, asc=10, hub=4, left=12, top=17, inset=4, radius=12.5, cut=1, track=1),
    # macOS grid (tile 52 of 64), for the icns 32 pt @2x slot
    "mac64": dict(size=64, bowl=16, stem=4, gap=4, asc=9, hub=4, left=14, top=19, inset=6, radius=11.5, cut=1, track=1),
    # in-app logo: 30 px header, 48 px About dialog, window icon
    "logo": dict(size=30, bowl=10, stem=3, gap=2, asc=6, hub=2.4, left=4, top=7, inset=0, radius=6.5,
                 round_hub=True),
}


def hinted_svg(key):
    p = dict(HINTED[key])
    n = p.pop("size", key)
    bowl, stem, gap, asc, hub = p["bowl"], p["stem"], p["gap"], p["asc"], p["hub"]
    cut, track = p.get("cut", 0), p.get("track", 0)
    ins = p["inset"]
    parts = [f'<path fill="{TILE}" d="{rounded(ins, ins, n - 2 * ins, n - 2 * ins, p["radius"])}"/>']
    for i, color in enumerate((WHITE, ORANGE)):
        ox = p["left"] + i * (bowl + gap)
        top = p["top"]
        by = top + asc
        cx, cy = ox + bowl / 2, by + bowl / 2
        sx = ox + bowl - stem
        if p.get("pixel"):
            platter = pixel_disk(ox, by, bowl)
            platter += rect(cx - hub / 2, cy - hub / 2, hub, hub)  # hub hole (evenodd)
        else:
            if cut:
                platter = disk_left_of(cx, cy, bowl / 2, sx - cut)
            else:
                platter = circle(cx, cy, bowl / 2)
            if track:
                rt = round(bowl * 0.5) / 2  # a little wider than the master: 1 px rings need room
                platter += ring_left_of(cx, cy, rt + 0.5, rt - 0.5, sx - cut if cut else cx + bowl)
            platter += (circle(cx, cy, hub / 2) if p.get("round_hub", hub > 2)
                        else rect(cx - hub / 2, cy - hub / 2, hub, hub))
        # platter and arm are separate paths: without a cut they overlap, and one
        # evenodd path would punch a hole where they meet
        parts.append(f'<path fill="{color}" fill-rule="evenodd" d="{platter}"/>')
        parts.append(f'<path fill="{color}" d="{rect(sx, top, stem, asc + bowl)}"/>')
    return svg(n, "\n".join(parts))


# ------------------------------------------------------------------ output
def rsvg(svg_text, size, out):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o", out],
                   input=svg_text.encode(), check=True)


def ico(pngs, out, png_from=128):
    """Windows .ico: 32-bit BMP entries below `png_from` px (what older loaders
    expect), PNG entries from there up (keeps the file, and the exe, small)."""
    images = []
    for size, path in pngs:
        if size >= png_from:
            data = open(path, "rb").read()
        else:
            rgba = subprocess.run(["magick", path, "-depth", "8", "RGBA:-"],
                                  capture_output=True, check=True).stdout
            assert len(rgba) == size * size * 4, path
            stride = size * 4
            xor, mask = bytearray(), bytearray()
            mask_stride = (size + 31) // 32 * 4
            for y in reversed(range(size)):  # DIBs are stored bottom-up
                row = bytearray(rgba[y * stride:(y + 1) * stride])
                row[0::4], row[2::4] = row[2::4], row[0::4]  # RGBA -> BGRA
                xor += row
                bits = bytearray(mask_stride)
                for x in range(size):
                    if rgba[y * stride + x * 4 + 3] == 0:
                        bits[x // 8] |= 0x80 >> (x % 8)
                mask += bits
            header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0,
                                 len(xor) + len(mask), 0, 0, 0, 0)
            data = header + bytes(xor) + bytes(mask)
        images.append((size, data))
    offset = 6 + 16 * len(images)
    directory, blobs = b"", b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blobs += data
    with open(out, "wb") as fh:
        fh.write(struct.pack("<HHH", 0, 1, len(images)) + directory + blobs)


def icns(entries, out):
    """entries: [(OSType, png_path)]. PNG payloads, readable by macOS 10.7 and later."""
    chunks = b""
    for ostype, path in entries:
        data = open(path, "rb").read()
        chunks += ostype.encode("ascii") + struct.pack(">I", len(data) + 8) + data
    with open(out, "wb") as fh:
        fh.write(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def main():
    png_dir = os.path.join(HERE, "png")
    os.makedirs(png_dir, exist_ok=True)

    master = master_svg()
    small = hinted_svg(32)
    write = lambda path, text: open(path, "w").write(text)
    write(os.path.join(HERE, "icon.svg"), master)
    write(os.path.join(HERE, "icon-small.svg"), small)
    write(os.path.join(ROOT, "ui", "logo.svg"), hinted_svg("logo"))

    pngs = {}
    for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        out = os.path.join(png_dir, f"DD-GUI-{size}.png")
        rsvg(hinted_svg(size) if size in HINTED else master, size, out)
        pngs[size] = out

    ico([(s, pngs[s]) for s in (16, 24, 32, 48, 64, 128, 256)], os.path.join(HERE, "DD-GUI.ico"))

    with tempfile.TemporaryDirectory() as tmp:
        mac = master_svg(inset=100)
        def mac_png(size):
            out = os.path.join(tmp, f"mac-{size}.png")
            rsvg(mac, size, out)
            return out
        mac64 = os.path.join(tmp, "mac-64h.png")
        rsvg(hinted_svg("mac64"), 64, mac64)
        m128, m256, m512, m1024 = (mac_png(s) for s in (128, 256, 512, 1024))
        icns([
            ("icp4", pngs[16]),   # 16
            ("icp5", pngs[32]),   # 32
            ("ic11", pngs[32]),   # 16@2x
            ("ic12", mac64),      # 32@2x
            ("ic07", m128),       # 128
            ("ic13", m256),       # 128@2x
            ("ic08", m256),       # 256
            ("ic14", m512),       # 256@2x
            ("ic09", m512),       # 512
            ("ic10", m1024),      # 512@2x
        ], os.path.join(HERE, "DD-GUI.icns"))
    print("icons written to", HERE)


if __name__ == "__main__":
    sys.exit(main())
