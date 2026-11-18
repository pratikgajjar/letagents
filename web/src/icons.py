#!/usr/bin/env python3
"""Render the site icons from the logo motif: two lit lamps joined by a dotted path that sags between them.

    python3 web/src/icons.py     # needs rsvg-convert and ImageMagick (magick)

Writes to web/: favicon.svg (follows the dark or light scheme), favicon.ico (16, 32, 48), apple-touch-icon.png
(180), icon-192.png, icon-512.png and icon-maskable-512.png. Run it after a change to the motif and commit the
files. CI does not run it.

Small sizes are drawn on their own 16-unit grid, with bigger lamps and three dots, so they stay clear at 16 px.
Large sizes use the logo geometry from gen.py, with a soft glow around each lamp.
"""
import os, subprocess, tempfile

WEB = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NIGHT, LIFT, AMBER, CREAM = "#0f0e0c", "#25211c", "#efb768", "#ece6da"


def arc_dots(A, C, B, n, inset):
    """n dots spaced evenly by arc length along the quadratic curve A-C-B, `inset` units in from each end."""
    q = lambda t: tuple((1 - t) ** 2 * A[i] + 2 * (1 - t) * t * C[i] + t ** 2 * B[i] for i in (0, 1))
    pts = [q(i / 400) for i in range(401)]
    acc = [0.0]
    for a, b in zip(pts, pts[1:]):
        acc.append(acc[-1] + ((b[0] - a[0]) ** 2 + (b[1] - a[1]) ** 2) ** .5)
    lo, hi = inset, acc[-1] - inset
    return [pts[min(range(401), key=lambda k: abs(acc[k] - (lo + (hi - lo) * i / (n - 1))))] for i in range(n)]


def small(scheme_css=True, tile=NIGHT, dots=CREAM):
    """16-unit grid: lamp A low left, lamp B high right, three path dots on the sag between them."""
    style = ""
    if scheme_css:
        # Light browser chrome: a night tile. Dark browser chrome: a slightly lifted tile, so its edge shows.
        style = (f"<style>.t{{fill:{NIGHT}}}.d{{fill:{CREAM}}}"
                 f"@media (prefers-color-scheme:dark){{.t{{fill:{LIFT}}}}}</style>")
    A, C, B = (3.5, 11.5), (8.5, 15.5), (12.5, 4.5)
    d = "".join(f'<circle cx="{x:.2f}" cy="{y:.2f}" r=".75"/>' for x, y in arc_dots(A, C, B, 3, 3.4))
    lamps = "".join(f'<circle cx="{x}" cy="{y}" r="3.1" fill="{AMBER}" opacity=".2"/><circle cx="{x}" cy="{y}" r="1.8" fill="{AMBER}"/>'
                    for x, y in (A, B))
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">' + style +
            f'<rect class="t" width="16" height="16" rx="3.5" fill="{tile}"/>{lamps}<g class="d" fill="{dots}">{d}</g></svg>')


def large(rounded, pad):
    """512 grid with the logo geometry (gen.py, 22-unit grid) scaled into a square that leaves `pad` free."""
    A, C, B = (3.5, 15.5), (11, 20), (18.5, 6.5)
    s = (512 - 2 * pad) / 22
    P = lambda p: (pad + p[0] * s, pad + p[1] * s)
    # Five dots spaced evenly by arc length between the lamp edges (lamp radius 2.3 units, plus a gap).
    dots = [f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{1.05 * s:.1f}"/>' for x, y in (P(p) for p in arc_dots(A, C, B, 5, 4.1))]
    (ax, ay), (bx, by) = P(A), P(B)
    lamp = lambda x, y: (f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{7.5 * s:.1f}" fill="url(#h)"/>'
                         f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{2.3 * s:.1f}" fill="{AMBER}"/>'
                         f'<circle cx="{x - .6 * s:.1f}" cy="{y - .6 * s:.1f}" r="{1.0 * s:.1f}" fill="#fff4dc" opacity=".55"/>')
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">'
        f'<defs><radialGradient id="h"><stop offset="0" stop-color="{AMBER}" stop-opacity=".42"/>'
        f'<stop offset=".35" stop-color="{AMBER}" stop-opacity=".14"/><stop offset="1" stop-color="{AMBER}" stop-opacity="0"/></radialGradient>'
        f'<radialGradient id="sky" cx=".72" cy=".2" r=".95"><stop offset="0" stop-color="#2a2620"/><stop offset="1" stop-color="{NIGHT}"/></radialGradient></defs>'
        f'<rect width="512" height="512" rx="{112 if rounded else 0}" fill="url(#sky)"/>'
        f'<g fill="{CREAM}" opacity=".62">{"".join(dots)}</g>'
        + lamp(ax, ay) + lamp(bx, by) + "</svg>"
    )


def png(svg, size, out):
    with tempfile.NamedTemporaryFile("w", suffix=".svg", delete=False) as f:
        f.write(svg)
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o", out, f.name], check=True)
    os.unlink(f.name)


def main():
    open(os.path.join(WEB, "favicon.svg"), "w").write(small() + "\n")
    tmp = tempfile.mkdtemp()
    parts = []
    for n in (16, 32, 48):
        p = os.path.join(tmp, f"{n}.png")
        png(small(scheme_css=False), n, p)
        parts.append(p)
    subprocess.run(["magick", *parts, os.path.join(WEB, "favicon.ico")], check=True)
    png(large(rounded=False, pad=96), 180, os.path.join(WEB, "apple-touch-icon.png"))
    png(large(rounded=True, pad=72), 192, os.path.join(WEB, "icon-192.png"))
    png(large(rounded=True, pad=72), 512, os.path.join(WEB, "icon-512.png"))
    # Maskable: full bleed, motif inside the central 80% safe circle.
    png(large(rounded=False, pad=128), 512, os.path.join(WEB, "icon-maskable-512.png"))
    for f in ("apple-touch-icon.png", "icon-192.png", "icon-512.png", "icon-maskable-512.png"):
        subprocess.run(["magick", os.path.join(WEB, f), "-strip", "-define", "png:compression-level=9", os.path.join(WEB, f)], check=True)
    print("wrote web/favicon.svg, favicon.ico, apple-touch-icon.png, icon-192.png, icon-512.png, icon-maskable-512.png")


if __name__ == "__main__":
    main()
