"""DBine's full logo (icon + wordmark) as single SVGs, horizontal and stacked.

Built from assets/dbine-icon.svg (the icon, unchanged) and the wordmarks in
web/public/brand/ (scripts/brand/wordmark.py). No dependencies:

    python3 scripts/brand/lockups.py
"""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BRAND = ROOT / "web/public/brand"


def inner(svg: str, prefix: str):
    """viewBox and body of an SVG, with its ids prefixed (no clashes once nested)."""
    vb = re.search(r'viewBox="([^"]+)"', svg).group(1)
    body = re.sub(r"^.*?<svg[^>]*>|</svg>\s*$", "", svg.strip(), flags=re.S)
    for i in set(re.findall(r'id="([^"]+)"', body)):
        body = body.replace(f'id="{i}"', f'id="{prefix}{i}"').replace(f"url(#{i})", f"url(#{prefix}{i})").replace(f'href="#{i}"', f'href="#{prefix}{i}"')
    return vb, body


def nest(svg, prefix, x, y, w, h):
    vb, body = inner(svg, prefix)
    return f'<svg x="{x}" y="{y}" width="{w}" height="{h}" viewBox="{vb}">{body}</svg>'


icon = (ROOT / "assets/dbine-icon.svg").read_text()
H = 1000  # icon size in the lockup's units
for variant in ("light", "dark", None):
    name = f"dbine-wordmark-{variant}.svg" if variant else "dbine-wordmark.svg"
    word = (BRAND / name).read_text()
    wvb = [float(v) for v in re.search(r'viewBox="([^"]+)"', word).group(1).split()]
    ratio = wvb[2] / wvb[3]
    suffix = f"-{variant}" if variant else ""
    # Horizontal: wordmark 0.68 of the icon's height, gap 0.1 (the icon has its own margin).
    wh = 0.68 * H
    ww = wh * ratio
    gap = 0.1 * H
    W = H + gap + ww
    out = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0f} {H}" role="img" aria-label="DBine">'
           + nest(icon, "i-", 0, 0, H, H) + nest(word, "w-", f"{H + gap:.0f}", f"{(H - wh) / 2:.0f}", f"{ww:.0f}", f"{wh:.0f}") + "</svg>\n")
    (BRAND / f"dbine-logo-horizontal{suffix}.svg").write_text(out)
    # Stacked: wordmark 0.4 of the icon's height, centered below.
    wh = 0.4 * H
    ww = wh * ratio
    gap = 0.12 * H
    W = max(H, ww)
    out = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0f} {H + gap + wh:.0f}" role="img" aria-label="DBine">'
           + nest(icon, "i-", f"{(W - H) / 2:.0f}", 0, H, H) + nest(word, "w-", f"{(W - ww) / 2:.0f}", f"{H + gap:.0f}", f"{ww:.0f}", f"{wh:.0f}") + "</svg>\n")
    (BRAND / f"dbine-logo-stacked{suffix}.svg").write_text(out)
print("ok")
