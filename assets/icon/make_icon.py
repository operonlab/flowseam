"""Lift the ink silhouette out of a generated logo-mark picture and set it on Flowseam's own tile.

    python3 assets/icon/make_icon.py assets/icon/source-otter.png assets/icon

The generators draw a dark mark on a cream tile on a white page. Only the mark is kept: its coverage is read
from luminance (cream = 0, ink = 1, anti-aliased edges and cut-out details in between), cropped to the ink's
bounding box, and placed on a squircle drawn here, so every candidate gets the same tile, size and colours.
Written: light-1024.png and dark-1024.png (the tile with its margin and shadow), icon-512.png and flowseam.ico (Windows
window, taskbar and file icons), and AppIcon.icns for the Mac bundle. The icns is a full-bleed square with no corners
of its own: macOS 26 and later round it themselves, and put an icon that is already rounded, with a margin, inside a
grey frame. Needs Pillow, numpy and macOS iconutil.
"""

import pathlib
import subprocess
import sys
import tempfile

import numpy as np
from PIL import Image, ImageFilter

CREAM = (243, 235, 222)
INK = (33, 30, 28)
TILE, CANVAS, MARK = 824, 1024, 0.66  # the mark's longer side, as a share of the tile

src, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
out.mkdir(parents=True, exist_ok=True)
lum = np.asarray(Image.open(src).convert("L")).astype(np.float32) / 255
cover = np.clip((0.78 - lum) / (0.78 - 0.18), 0, 1)  # cream page/tile ~0.9+, ink ~0.1
ink = cover > 0.5
rows, cols = np.where(ink.any(axis=1))[0], np.where(ink.any(axis=0))[0]
if len(rows) == 0:
    sys.exit("no ink found")
top, bottom, left, right = rows[0], rows[-1], cols[0], cols[-1]
h, w = bottom - top + 1, right - left + 1
if max(h, w) > 0.9 * min(lum.shape):
    sys.exit(f"ink spans {w}x{h} of {lum.shape[1]}x{lum.shape[0]}: the tile itself is dark, check by eye")
pad = round(0.02 * max(h, w))
crop = cover[max(top - pad, 0) : bottom + pad + 1, max(left - pad, 0) : right + pad + 1]
mark = Image.fromarray((crop * 255).astype(np.uint8))
scale = MARK * TILE / max(mark.size)
mark = mark.resize((round(mark.width * scale), round(mark.height * scale)), Image.Resampling.LANCZOS)


def squircle(size, exponent=5.0, ss=4):
    s = size * ss
    t = (np.arange(s) + 0.5) / s * 2 - 1
    x, y = np.meshgrid(t, t)
    m = (np.abs(x) ** exponent + np.abs(y) ** exponent) <= 1
    return Image.fromarray((m * 255).astype(np.uint8)).resize((size, size), Image.Resampling.LANCZOS)


shape = squircle(TILE)
off = (CANVAS - TILE) // 2


def icon(bg, fg):
    tile = Image.new("RGBA", (TILE, TILE), bg + (255,))
    tile.paste(Image.new("RGBA", mark.size, fg + (255,)), ((TILE - mark.width) // 2, (TILE - mark.height) // 2), mark)
    tile.putalpha(shape)
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    shadow = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    shadow.paste(Image.new("RGBA", (TILE, TILE), (0, 0, 0, 70)), (off, off + 10), shape)
    canvas.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(12)))
    canvas.alpha_composite(tile, (off, off))
    return canvas


light, dark = icon(CREAM, INK), icon(INK, CREAM)
full = Image.new("RGBA", (CANVAS, CANVAS), CREAM + (255,))
big = mark.resize((round(mark.width * CANVAS / TILE), round(mark.height * CANVAS / TILE)), Image.Resampling.LANCZOS)
full.paste(Image.new("RGBA", big.size, INK + (255,)), ((CANVAS - big.width) // 2, (CANVAS - big.height) // 2), big)
light.save(out / "light-1024.png")
dark.save(out / "dark-1024.png")
with tempfile.TemporaryDirectory() as d:
    iconset = pathlib.Path(d) / "AppIcon.iconset"
    iconset.mkdir()
    for base in (16, 32, 128, 256, 512):
        full.resize((base, base), Image.Resampling.LANCZOS).save(iconset / f"icon_{base}x{base}.png")
        full.resize((base * 2, base * 2), Image.Resampling.LANCZOS).save(iconset / f"icon_{base}x{base}@2x.png")
    subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out / "AppIcon.icns")], check=True)
light.resize((512, 512), Image.Resampling.LANCZOS).save(out / "icon-512.png")
light.save(out / "flowseam.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print(f"mark {w}x{h} at ({left},{top}) -> {out}")
