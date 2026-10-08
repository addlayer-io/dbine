"""DBine's wordmark ("DBine" with the champagne halo on the i) as SVG paths.

Source of web/public/brand/*.svg. Needs fontTools and Quicksand's variable
font (SIL Open Font License; https://github.com/google/fonts/tree/main/ofl/quicksand):

    python3 -m venv /tmp/fontenv && /tmp/fontenv/bin/pip install fonttools
    /tmp/fontenv/bin/python scripts/brand/wordmark.py Quicksand[wght].ttf web/public/brand
"""
import sys
FONT, OUT = sys.argv[1], sys.argv[2]
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
import math
base=TTFont(FONT)
def inst(w):
    return instantiateVariableFont(TTFont(FONT), {"wght": w})
f700, f500 = inst(700), inst(500)
upm = base["head"].unitsPerEm
def run(font, text, x0):
    cmap=font.getBestCmap(); gs=font.getGlyphSet(); hmtx=font["hmtx"]
    pen=SVGPathPen(gs); x=x0; starts=[]
    kern={}
    for ch in text:
        g=cmap[ord(ch)]; starts.append((x,hmtx[g][0]))
        tp=TransformPen(pen,(1,0,0,-1,x,0)); gs[g].draw(tp); x+=hmtx[g][0]
    return pen.getCommands(), x, starts
db, x1, _ = run(f700, "DB", 0)
ine, x2, st = run(f500, "ıne", x1 + 10)
ix, iw = st[0]; cx = ix + iw/2
xh = f500["OS/2"].sxHeight
cy = -(xh + 0.17*upm)
rx, ry, sw = 0.24*upm, 0.095*upm, 0.07*upm
asc = f700["OS/2"].sTypoAscender if hasattr(f700["OS/2"],"sTypoAscender") else 800
top = cy - ry - sw*2; bottom = 0.03*upm
pad = 20
minx, maxx = -pad, x2 + pad
h = bottom - top + pad
def svg(deep, lite, gold=("#f6e7b4","#b8913a")):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="{minx:.0f} {top-pad/2:.0f} {maxx-minx:.0f} {h:.0f}" role="img" aria-label="DBine">
<defs><linearGradient id="dbine-halo" x1="0" x2="1"><stop offset="0" stop-color="{gold[0]}"/><stop offset="1" stop-color="{gold[1]}"/></linearGradient></defs>
<path fill="{deep}" d="{db}"/>
<path fill="{lite}" d="{ine}"/>
<ellipse cx="{cx:.1f}" cy="{cy:.1f}" rx="{rx:.1f}" ry="{ry:.1f}" transform="rotate(-12 {cx:.1f} {cy:.1f})" fill="none" stroke="url(#dbine-halo)" stroke-width="{sw:.1f}"/>
</svg>
'''
open(OUT+"/dbine-wordmark-light.svg","w").write(svg("#2448c8","#4f9df5"))
open(OUT+"/dbine-wordmark-dark.svg","w").write(svg("#6d98ff","#a6d3ff"))
open(OUT+"/dbine-wordmark.svg","w").write(svg("var(--dbine-deep, #2448c8)","var(--dbine-lite, #4f9df5)"))
print("ok", round(maxx-minx), round(h))
