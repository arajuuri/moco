#!/usr/bin/env python3
"""Turn the JSON written by `MOCO_GALLERY=dir cargo test -p moco gallery` into PNG screenshots.

Usage: tui-gallery.py scenes.json OUTDIR [scene-name ...]
Colors follow a typical terminal palette (named ANSI colors are the terminal's choice, so this is an approximation)."""
import json, sys, os
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
ITALIC = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Oblique.ttf"
SIZE = 15
f, fb, fi = (ImageFont.truetype(p, SIZE) for p in (FONT, BOLD, ITALIC))
CW = int(f.getlength("M")) + 0
CH = SIZE + 5

ANSI = {"Black": (0, 0, 0), "Red": (205, 49, 49), "Green": (13, 188, 121), "Yellow": (229, 229, 16), "Blue": (36, 114, 200),
        "Magenta": (188, 63, 188), "Cyan": (17, 168, 205), "Gray": (204, 204, 204), "DarkGray": (118, 118, 118),
        "LightRed": (241, 76, 76), "LightGreen": (35, 209, 139), "LightYellow": (245, 245, 67), "LightBlue": (59, 142, 234),
        "LightMagenta": (214, 112, 214), "LightCyan": (41, 184, 219), "White": (255, 255, 255)}

def xterm(i):
    if i < 16:
        names = ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "Gray", "DarkGray", "LightRed", "LightGreen", "LightYellow", "LightBlue", "LightMagenta", "LightCyan", "White"]
        return ANSI[names[i]]
    if i >= 232:
        v = 8 + 10 * (i - 232)
        return (v, v, v)
    i -= 16
    steps = [0, 95, 135, 175, 215, 255]
    return (steps[i // 36], steps[(i // 6) % 6], steps[i % 6])

def rgb(c, default):
    if c == "reset":
        return default
    kind, _, v = c.partition(":")
    if kind == "named":
        return ANSI[v]
    if kind == "idx":
        return xterm(int(v))
    return tuple(int(x) for x in v.split(","))

def render(scene, light):
    dfg, dbg = ((30, 30, 30), (253, 253, 253)) if light else ((204, 204, 204), (30, 30, 30))
    w, h = scene["w"], scene["h"]
    img = Image.new("RGB", (w * CW + 16, h * CH + 16), dbg)
    d = ImageDraw.Draw(img)
    for n, (sym, fg, bg, mods) in enumerate(scene["cells"]):
        x, y = n % w, n // w
        fgc, bgc = rgb(fg, dfg), rgb(bg, dbg)
        if mods & 0x40:  # REVERSED
            fgc, bgc = bgc, fgc
        if mods & 0x04:  # DIM
            fgc = tuple((a + b) // 2 for a, b in zip(fgc, bgc))
        px, py = 8 + x * CW, 8 + y * CH
        if bgc != dbg:
            d.rectangle([px, py, px + CW, py + CH], fill=bgc)
        if sym.strip():
            font = fb if mods & 0x01 else fi if mods & 0x10 else f
            d.text((px, py + 1), sym, font=font, fill=fgc)
            if mods & 0x08:  # UNDERLINED
                d.line([px, py + CH - 2, px + CW, py + CH - 2], fill=fgc)
    return img

if __name__ == "__main__":
    scenes = json.load(open(sys.argv[1]))
    out = sys.argv[2]
    os.makedirs(out, exist_ok=True)
    only = sys.argv[3:]
    for name, sc in scenes.items():
        if only and name not in only:
            continue
        render(sc, name.startswith("light")).save(os.path.join(out, name + ".png"))
        print(name)
