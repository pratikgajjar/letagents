# Generates the one diagram on web/index.html: machines, agents and addresses, the relay, and one
# animated ask and reply (CSS keyframes only; positions are computed here).
W, H = 980, 400
T = 16  # seconds per loop


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


out = []
add = out.append

# relay
RX, RY, RW, RH = 395, 70, 190, 100
add(f'<rect class="relayzone" x="{RX}" y="{RY}" width="{RW}" height="{RH}"/>')
add(f'<text x="{RX+RW/2}" y="{RY+24}" text-anchor="middle" font-weight="800">letagents.talk relay</text>')
add(f'<rect class="hl" x="{RX+20}" y="{RY+36}" width="{RW-40}" height="20"/>')
add(f'<text x="{RX+RW/2}" y="{RY+50}" text-anchor="middle" font-weight="800">sealed bytes only</text>')
add(f'<text x="{RX+RW/2}" y="{RY+74}" text-anchor="middle" class="sub">no sender, no keys</text>')
add(f'<text x="{RX+RW/2}" y="{RY+90}" text-anchor="middle" class="sub">kept 48 h at most</text>')

MW = 300
machines = {
    "laptop": (10, 10, [("laptop/claude-1048c372", "Claude Code", True), ("laptop/codex-42619", "Codex", False), ("laptop/cloud-bridge", "pi", False)]),
    "devbox": (670, 10, [("devbox/planner", "pi", True), ("devbox/reviewer", "pi", False), ("devbox/docs-bot", "pi", False)]),
    "teammate": (340, 248, [("teammate/claude-7e21b9aa", "Claude Code", False), ("teammate/reviewer", "pi", False)]),
}
pos = {}
for name, (mx, my, agents) in machines.items():
    mh = 30 + len(agents) * 30 + 44
    add(f'<rect class="zone" x="{mx}" y="{my}" width="{MW}" height="{mh}"/>')
    add(f'<text x="{mx+10}" y="{my+18}" class="zl">MACHINE {name.upper()}</text>')
    add(f'<text x="{mx+MW-10}" y="{my+18}" text-anchor="end" class="zs">plaintext stays here</text>')
    for i, (addr, harness, hl) in enumerate(agents):
        ay = my + 28 + i * 30
        add(f'<rect class="{"hl" if hl else "node"}" x="{mx+10}" y="{ay}" width="{MW-20}" height="24"/>')
        add(f'<text x="{mx+18}" y="{ay+16}" font-weight="800">{esc(addr)}</text>')
        add(f'<text x="{mx+MW-18}" y="{ay+16}" text-anchor="end" class="zs">{esc(harness)}</text>')
        pos[addr] = (mx + MW / 2, ay + 12)
    dy = my + 28 + len(agents) * 30 + 6
    add(f'<rect class="node daemon" x="{mx+10}" y="{dy}" width="{MW-20}" height="26"/>')
    add(f'<text x="{mx+MW/2}" y="{dy+17}" text-anchor="middle" font-weight="800">daemon: keys · rules · seal / open</text>')
    pos[name] = (mx + MW / 2, dy + 13, mx, dy)

# sealed links: each daemon to the relay
for name, edge in (("laptop", (RX, RY + RH / 2)), ("devbox", (RX + RW, RY + RH / 2))):
    _, _, mx, dy = pos[name]
    x0 = mx + MW - 10 if name == "laptop" else mx + 10
    add(f'<polyline class="sealed" fill="none" points="{x0},{dy+13} {(x0+edge[0])/2},{dy+13} {(x0+edge[0])/2},{edge[1]} {edge[0]},{edge[1]}"/>')
_, _, tmx, tdy = pos["teammate"]
add(f'<line class="sealed" x1="{RX+RW/2}" y1="{RY+RH}" x2="{RX+RW/2}" y2="{machines["teammate"][1]}"/>')

# animated ask (laptop/claude -> devbox/planner) and reply
A = pos["laptop/claude-1048c372"][:2]
DA = pos["laptop"][:2]
R = (RX + RW / 2, RY + RH / 2)
DB = pos["devbox"][:2]
B = pos["devbox/planner"][:2]
CW, CHH = 168, 22
variants = {"qp": ("are tests green?", "plain"), "qc": ("LA1 5f0e9b…c91a", "cipher"),
            "rp": ("yes, all green", "plain"), "rc": ("LA1 a71d03…4be2", "cipher")}
steps = [(0, A, "qp"), (6, A, "qp"), (13, DA, "qp"), (17, DA, "qc"), (26, R, "qc"), (30, R, "qc"), (39, DB, "qc"),
         (43, DB, "qp"), (49, B, "qp"), (53, B, "qp"), (55, B, "rp"), (59, B, "rp"), (65, DB, "rp"), (69, DB, "rc"),
         (78, R, "rc"), (82, R, "rc"), (91, DA, "rc"), (95, DA, "rp"), (100, A, "rp")]
css = ["@keyframes la-move {" + " ".join(f"{p}% {{ transform: translate({x-CW/2:.1f}px, {y-CHH/2:.1f}px); }}" for p, (x, y), _ in steps) + "}"]
for v in variants:
    css.append(f"@keyframes la-{v} {{" + " ".join(f"{p}% {{ opacity: {1 if vv == v else 0}; }}" for p, _, vv in steps) + "}")
css.append(f".la-chip {{ animation: la-move {T}s linear infinite; }}")
css += [f".la-{v} {{ animation: la-{v} {T}s linear infinite; }}" for v in variants]
css.append("@media (prefers-reduced-motion: reduce) { .la-chip { display: none; } }")
add(f"<style>{' '.join(css)}</style>")
add('<g class="la-chip">')
for v, (txt, kind) in variants.items():
    add(f'<g class="la-{v}" style="opacity:0"><rect class="chip-{kind}" x="0" y="0" width="{CW}" height="{CHH}"/>'
        f'<text x="{CW/2}" y="15" text-anchor="middle" class="chip-{kind}-t">{esc(txt)}</text></g>')
add("</g>")

svg = (f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="Three machines (laptop, devbox, teammate), each with several agents '
       f'addressed as machine/agent and one daemon, all connected to the letagents.talk relay, which holds sealed bytes only. '
       f'An animated question goes from laptop/claude-1048c372 to devbox/planner: plaintext on the laptop, sealed by the '
       f'laptop daemon, sealed bytes through the relay, opened by the devbox daemon, and the reply returns the same way.">\n'
       + "\n".join(out) + "\n</svg>")
