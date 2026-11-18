#!/usr/bin/env python3
"""Render the social preview images (the night painting beside one sentence) with headless Chromium.
They are rendered at 1x: the painting is grainy, so a 2x PNG is about 1 MB, and the relay bundles them.

    python3 web/src/social.py        # writes web/og.png (1200x630) and web/x.png (1200x600)

Run it after a change to the painting (web/art-night.webp) or the copy below, and commit the PNGs. CI does
not run it.
"""
import os, runpy, subprocess, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CHROME = os.environ.get("CHROME", "/Applications/Chromium.app/Contents/MacOS/Chromium")
SIZES = {"og.png": (1200, 630), "x.png": (1200, 600)}  # Open Graph (Facebook, LinkedIn, Slack, ...), X
ART = "file://" + os.path.join(ROOT, "web", "art-night.webp")
MARK = runpy.run_path(os.path.join(ROOT, "web", "src", "gen.py"), run_name="gen")["mark"]


def card(w, h):
    side = h - 96
    return f"""<!doctype html><html><head><meta charset="utf-8"><style>
  :root {{ --amber: #e9b46a; }}
  html, body {{ width: {w}px; height: {h}px; overflow: hidden; margin: 0; background: #0f0e0c; color: #bdb4a6;
    font-family: -apple-system, "Helvetica Neue", Arial, sans-serif; -webkit-font-smoothing: antialiased; }}
  .c {{ display: grid; grid-template-columns: minmax(0, 1fr) {side}px; gap: 64px; align-items: center; height: {h}px; padding: 0 56px 0 72px; box-sizing: border-box; }}
  .brand {{ display: flex; align-items: center; gap: 12px; font: 400 26px/1 "Iowan Old Style", Palatino, Georgia, serif; color: #ece6da; }}
  .brand svg {{ width: 30px; height: 30px; color: #ece6da; }}
  h1 {{ font: 400 40px/1.24 "Iowan Old Style", Palatino, Georgia, serif; color: #ece6da; margin: 40px 0 0; letter-spacing: -.01em; text-wrap: balance; }}
  .cmd {{ margin-top: 40px; padding: 14px 0; border-top: 1px solid #2b2824; border-bottom: 1px solid #2b2824;
    font: 400 15px/1.5 ui-monospace, "SF Mono", Menlo, monospace; color: #ece6da; white-space: nowrap; }}
  .cmd span {{ color: #8f8678; }}
  img {{ width: {side}px; height: {side}px; display: block; }}
</style></head><body><div class="c">
  <div>
    <div class="brand">{MARK}letagents.talk</div>
    <h1>Let your AI agents on different machines talk to each other: end&#8209;to&#8209;end encrypted, no VPN, no open port.</h1>
    <div class="cmd"><span>$ </span>curl -fsSL letagents.talk/install.sh | sh -s -- invite</div>
  </div>
  <img src="{ART}" alt="">
</div></body></html>"""


with tempfile.TemporaryDirectory() as tmp:
    for name, (w, h) in SIZES.items():
        src = os.path.join(tmp, name + ".html")
        open(src, "w").write(card(w, h))
        out = os.path.join(ROOT, "web", name)
        subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1",
                        "--allow-file-access-from-files", f"--window-size={w},{h}", f"--screenshot={out}", "file://" + src],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print(f"wrote web/{name} ({w}x{h})")
