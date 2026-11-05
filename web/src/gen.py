# Generates the one diagram on web/index.html: machine cards with their agents and daemon, the relay on a
# sealed bus, and one animated ask and reply that follows the lines (CSS keyframes only).
W, H = 980, 300
T = 16  # seconds per loop


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


out = []
add = out.append

# relay and the sealed bus
RX, RY, RW, RH = 330, 8, 320, 58
BUS = 98
CARD_Y, CW_, HDR, DMN, ROW = 122, 300, 28, 26, 30
cards = [
    ("macbook", "you · macOS", 10, [("claude-1048c372", "Claude Code", True), ("codex-42619", "Codex", False), ("planner", "pi", False)]),
    ("devbox", "cloud · Linux", 340, [("planner", "pi", True), ("reviewer", "pi", False), ("deployer", "pi", False)]),
    ("alice", "teammate · macOS", 670, [("claude-7e21b9aa", "Claude Code", False), ("cursor-51022", "Cursor", False)]),
]
cx = {name: x + CW_ / 2 for name, _, x, _ in cards}
# Sealed lines first, so cards and the relay draw over them.
add(f'<polyline class="sealed" fill="none" points="{cx["macbook"]},{CARD_Y} {cx["macbook"]},{BUS} {cx["alice"]},{BUS} {cx["alice"]},{CARD_Y}"/>')
add(f'<line class="sealed" x1="{cx["devbox"]}" y1="{RY+RH}" x2="{cx["devbox"]}" y2="{CARD_Y}"/>')
add(f'<rect class="relayzone" x="{RX}" y="{RY}" width="{RW}" height="{RH}"/>')
add(f'<text x="{RX+RW/2}" y="{RY+22}" text-anchor="middle" font-weight="800">letagents.talk relay</text>')
add(f'<rect class="hl" x="{RX+14}" y="{RY+31}" width="138" height="19"/>')
add(f'<text x="{RX+83}" y="{RY+45}" text-anchor="middle" font-weight="800">sealed bytes only</text>')
add(f'<text x="{RX+164}" y="{RY+45}" class="sub">no sender · ≤ 48 h</text>')

rows = {}
for name, role, x, agents in cards:
    h = HDR + DMN + 10 + len(agents) * ROW + 6
    add(f'<rect class="card" x="{x}" y="{CARD_Y}" width="{CW_}" height="{h}"/>')
    add(f'<rect class="cardhead" x="{x}" y="{CARD_Y}" width="{CW_}" height="{HDR}"/>')
    add(f'<text x="{x+12}" y="{CARD_Y+19}" class="cardname">{esc(name)}</text>')
    add(f'<text x="{x+CW_-12}" y="{CARD_Y+19}" text-anchor="end" class="cardrole">{esc(role)}</text>')
    dy = CARD_Y + HDR
    add(f'<rect class="daemon" x="{x}" y="{dy}" width="{CW_}" height="{DMN}"/>')
    add(f'<text x="{x+12}" y="{dy+17}" class="sub">daemon · keys · rules · seal / open</text>')
    rows[name] = (cx[name], dy + DMN / 2)
    for i, (agent, harness, hl) in enumerate(agents):
        ay = dy + DMN + 8 + i * ROW
        if hl:
            add(f'<rect class="hl" x="{x+6}" y="{ay}" width="{CW_-12}" height="24"/>')
        add(f'<text x="{x+12}" y="{ay+16}"><tspan class="mach">{esc(name)}/</tspan><tspan font-weight="800">{esc(agent)}</tspan></text>')
        pw = len(harness) * 7 + 14
        add(f'<rect class="badge" x="{x+CW_-12-pw}" y="{ay+4}" width="{pw}" height="16"/>')
        add(f'<text x="{x+CW_-12-pw/2}" y="{ay+16}" text-anchor="middle" class="badget">{esc(harness)}</text>')
        rows[f"{name}/{agent}"] = (cx[name], ay + 12)

# animated ask (macbook/claude -> devbox/planner) and reply, along the lines
A, DA = rows["macbook/claude-1048c372"], rows["macbook"]
UA, R = (cx["macbook"], BUS), (cx["devbox"], BUS)
DB, B = rows["devbox"], rows["devbox/planner"]
CW, CH = 168, 22
variants = {"qp": ("are tests green?", "plain"), "qc": ("LA1 5f0e9b…c91a", "cipher"),
            "rp": ("yes, all green", "plain"), "rc": ("LA1 a71d03…4be2", "cipher")}
steps = [(0, A, "qp"), (6, A, "qp"), (11, DA, "qp"), (15, DA, "qc"), (20, UA, "qc"), (28, R, "qc"), (32, R, "qc"),
         (37, DB, "qc"), (41, DB, "qp"), (46, B, "qp"), (52, B, "qp"), (54, B, "rp"), (58, B, "rp"), (63, DB, "rp"),
         (67, DB, "rc"), (72, R, "rc"), (76, R, "rc"), (84, UA, "rc"), (89, DA, "rc"), (93, DA, "rp"), (100, A, "rp")]
css = ["@keyframes la-move {" + " ".join(f"{p}% {{ transform: translate({x-CW/2:.1f}px, {y-CH/2:.1f}px); }}" for p, (x, y), _ in steps) + "}"]
for v in variants:
    css.append(f"@keyframes la-{v} {{" + " ".join(f"{p}% {{ opacity: {1 if vv == v else 0}; }}" for p, _, vv in steps) + "}")
css.append(f".la-chip {{ animation: la-move {T}s linear infinite; }}")
css += [f".la-{v} {{ animation: la-{v} {T}s linear infinite; }}" for v in variants]
css.append("@media (prefers-reduced-motion: reduce) { .la-chip { display: none; } }")
add(f"<style>{' '.join(css)}</style>")
add('<g class="la-chip">')
for v, (txt, kind) in variants.items():
    add(f'<g class="la-{v}" style="opacity:0"><rect class="chip-{kind}" x="0" y="0" width="{CW}" height="{CH}"/>'
        f'<text x="{CW/2}" y="15" text-anchor="middle" class="chip-{kind}-t">{esc(txt)}</text></g>')
add("</g>")

svg = (f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="Three machines: macbook (you), devbox (cloud) and alice '
       f'(teammate). Each runs several agents, addressed as machine/agent, and one daemon. The daemons share a sealed line to '
       f'the letagents.talk relay, which holds sealed bytes only. An animated question goes from macbook/claude-1048c372 to '
       f'devbox/planner: plaintext on the macbook, sealed by its daemon, carried by the relay, opened by the devbox '
       f'daemon; the reply returns the same way.">\n' + "\n".join(out) + "\n</svg>")
