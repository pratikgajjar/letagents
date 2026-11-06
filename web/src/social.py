#!/usr/bin/env python3
"""Render the social preview images from web/index.html (its style and diagram) with headless Chromium.

    python3 web/src/social.py        # writes web/og.png (1200x630) and web/x.png (1200x600), at 2x

Run it after a change to the page diagram or the copy below, and commit the PNGs. CI does not run it.
"""
import os, re, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CHROME = os.environ.get("CHROME", "/Applications/Chromium.app/Contents/MacOS/Chromium")
SIZES = {"og.png": (1200, 630), "x.png": (1200, 600)}  # Open Graph (Facebook, LinkedIn, Slack, ...), X

html = open(os.path.join(ROOT, "web/index.html")).read()
style = re.search(r"<style>.*?</style>", html, re.S).group(0)
svg = re.search(r"<svg viewBox.*?</svg>", html, re.S).group(0)


def card(w, h):
    return f"""<!doctype html><html><head><meta charset="utf-8">{style}
<style>
  html, body {{ width: {w}px; height: {h}px; overflow: hidden; margin: 0; background: #fff; }}
  .xc {{ width: {w}px; height: {h}px; padding: 0 56px; display: flex; flex-direction: column; justify-content: center; gap: 14px; }}
  .top {{ display: flex; justify-content: space-between; align-items: baseline; }}
  .t {{ font-size: 44px; font-weight: 800; line-height: 1; margin: 0; }}
  .url {{ font-size: 20px; font-weight: 800; color: var(--blue); }}
  .lead {{ font-size: 23px; line-height: 1.35; margin: 0; max-width: none; }}
  .lead mark {{ font-weight: 800; }}
  pre.cmd {{ font-size: 16.5px; line-height: 1.55; margin: 0; padding: 10px 16px; flex: none; }}
  pre.cmd .c {{ color: var(--mute); }}
  .fig {{ border: 1.5px solid var(--line); padding: 8px 12px; margin: 0; flex: none; }}
  .fig svg {{ width: {w - 140}px; height: auto; overflow: visible; margin: 0 auto; }}
  * {{ animation: none !important; }}
</style></head><body><div class="xc">
  <div class="top"><h1 class="t">letagents</h1><span class="url">letagents.talk</span></div>
  <p class="lead">AI agents on different machines, talking. <mark>No Tailscale, no VPN, no open port.</mark></p>
  <pre class="cmd"><code><span class="c">A $</span> curl -fsSL https://letagents.talk/install.sh | sh -s -- invite
<span class="c">B $</span> curl -fsSL https://letagents.talk/install.sh | sh -s -- join macbook la1.…</code></pre>
  <div class="fig">{svg}</div>
</div></body></html>"""


with tempfile.TemporaryDirectory() as tmp:
    for name, (w, h) in SIZES.items():
        src = os.path.join(tmp, name + ".html")
        open(src, "w").write(card(w, h))
        out = os.path.join(ROOT, "web", name)
        subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=2",
                        f"--window-size={w},{h}", f"--screenshot={out}", "file://" + src],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print(f"wrote web/{name} ({2 * w}x{2 * h})")
