#!/usr/bin/env python3
"""Generate the braille art of oxutrm's startup splash.

The ox head is rasterised from the oxulnk logo's SVG paths (copied below,
with the SVG's own translate), and the name is drawn in a small hand-made dot
font in the style of the logo's lettering. Output: Rust `const` arrays, to be
pasted into crates/oxutrm-client/src/splash.rs.

    python3 tools/oxlogo.py > /tmp/art.rs

Braille: one character cell holds 2x4 dots, and a terminal cell is about
twice as tall as it is wide, so a dot is square and the art keeps the logo's
proportions. Blank cells are U+2800 (empty braille); the splash draws those as
spaces.
"""
import re

# The oxulnk ox head, from its SVG (viewBox 0 0 159.13449 124.66308,
# transform translate(192.54433,-10.142128)). Only m/l/h/v/z, all relative.
PATHS = [
    "m -127.26459,108.21458 -9.525,7.9375 v 10.58333 l 7.9375,7.9375 h 15.875 15.875004 l 7.9375,-7.9375 v -10.58333 l -9.525,-7.9375 h -14.287504 z",
    "m -75.935416,60.589582 v 10.58333 h 16.404167 l 7.9375,-7.9375 z",
    "m -150.01875,60.589582 -24.34167,2.64583 7.9375,7.9375 h 16.40417 z",
    "m -138.37709,37.835412 -8.99583,1.5875 v 56.62084 l 10.58333,16.933328 7.9375,-6.87917 -2.64583,-26.987498 -10.58333,-2.64583 v -10.58333 l 13.22916,10.58333 2.64584,29.104168 h 13.22916 13.229174 l 2.64583,-29.104168 13.22917,-10.58333 v 10.58333 l -10.58333,2.64583 -2.64584,26.987498 7.9375,6.87917 10.58334,-16.933328 v -56.62084 l -8.99584,-1.5875 -7.40833,25.4 h -17.991674 -17.99166 z",
    "m -136.78959,33.072912 7.9375,27.25209 h 15.875 15.875004 l 7.9375,-27.25209 h -23.812504 z",
    "m -52.122916,10.318752 -5.291667,2.64583 5.291667,23.8125 -23.8125,2.64583 v 18.52084 l 42.333333,-11.90625 z",
    "m -173.83125,10.318752 -18.52084,35.71875 42.33334,11.90625 v -18.52084 l -23.8125,-2.64583 5.29166,-23.8125 z",
]
TX, TY = 192.54433, -10.142128
W, H = 159.13449, 124.66308

def polygons():
    polys = []
    for d in PATHS:
        toks = re.findall(r"[a-zA-Z]|-?\d*\.?\d+", d)
        x = y = 0.0
        pts, cmd, i = [], None, 0
        while i < len(toks):
            if toks[i].isalpha():
                cmd = toks[i]
                i += 1
            if cmd == "z":
                break
            if cmd in "ml":
                x += float(toks[i]); y += float(toks[i + 1]); i += 2
                pts.append((x, y))
                cmd = "l"
            elif cmd == "h":
                x += float(toks[i]); i += 1; pts.append((x, y))
            elif cmd == "v":
                y += float(toks[i]); i += 1; pts.append((x, y))
        polys.append([(px + TX, py + TY) for px, py in pts])
    return polys

POLYS = polygons()

def inside(px, py):
    for p in POLYS:
        c = False
        for k in range(len(p)):
            (x1, y1), (x2, y2) = p[k], p[(k + 1) % len(p)]
            if (y1 > py) != (y2 > py) and px < x1 + (py - y1) * (x2 - x1) / (y2 - y1):
                c = not c
        if c:
            return True
    return False

# The name: cap height 12 dots, x-height 8, strokes 2 dots.
GLYPHS = {
    "O": [".########.", "##########"] + ["##......##"] * 8 + ["##########", ".########."],
    "x": ["##....##", "###..###", ".######.", "..####..", "..####..", ".######.", "###..###", "##....##"],
    "u": ["##....##"] * 6 + ["########", ".#######"],
    "T": ["##########", "##########"] + ["....##...."] * 10,
    "r": ["##.###", "######", "###...", "##...."] + ["##...."] * 4,
    "m": ["#########.", "##########"] + ["##..##..##"] * 6,
}
# The T's bar overhangs its stem, so the usual gap on either side reads as a hole.
KERN = {("u", "T"): 0, ("T", "r"): 0}
GAP = 2

def word(text, h=12):
    rows = [""] * h
    for i, ch in enumerate(text):
        g = GLYPHS[ch]
        w, pad = len(g[0]), h - len(g)
        gap = KERN.get((ch, text[i + 1]), GAP) if i + 1 < len(text) else 0
        for y in range(h):
            rows[y] += (g[y - pad] if y >= pad else "." * w) + "." * gap
    return [[c == "#" for c in r] for r in rows]

BITS = [(0, 0, 1), (0, 1, 2), (0, 2, 4), (1, 0, 8), (1, 1, 16), (1, 2, 32), (0, 3, 64), (1, 3, 128)]

def to_braille(grid):
    w = len(grid[0])
    out = []
    for r in range(len(grid) // 4):
        s = ""
        for c in range(w // 2):
            s += chr(0x2800 + sum(b for bx, by, b in BITS if grid[r * 4 + by][c * 2 + bx]))
        out.append(s)
    return out

HEAD_COLS = 32

def head():
    dw = HEAD_COLS * 2
    dh = int(round(dw * H / W))
    dh += (-dh) % 4
    return [[inside((x + .5) * W / dw, (y + .5) * H / dh) for x in range(dw)] for y in range(dh)]

def name():
    g = word("OxuTrm")
    dw = HEAD_COLS * 2
    off = (dw - len(g[0])) // 2 + 1
    off += off % 2  # nudged right of centre, by whole cells only
    return [[0 <= x - off < len(r) and r[x - off] for x in range(dw)] for r in g] + [[False] * dw] * 0

def rust(name_, lines):
    body = "".join(f'    "{l}",\n' for l in lines)
    return f"pub(crate) const {name_}: [&str; {len(lines)}] = [\n{body}];\n"

if __name__ == "__main__":
    print("// Generated by tools/oxlogo.py -- edit that, not this.")
    print(rust("HEAD", to_braille(head())))
    print(rust("NAME", to_braille(name())))
