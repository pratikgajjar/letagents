# Generates the two inline SVG diagrams for web/index.html. Coordinates are computed, not placed by eye.
W = 980
def esc(s): return s.replace("&","&amp;").replace("<","&lt;").replace(">","&gt;")
def box(x,y,w,h,lines,cls="node",bold_first=True):
    out=[f'<rect class="{cls}" x="{x}" y="{y}" width="{w}" height="{h}"/>']
    n=len(lines); lh=15; y0=y+h/2-(n-1)*lh/2+4
    for i,t in enumerate(lines):
        wt=' font-weight="800"' if (i==0 and bold_first) else ' class="sub"'
        out.append(f'<text x="{x+w/2}" y="{y0+i*lh}" text-anchor="middle"{wt}>{esc(t)}</text>')
    return "\n".join(out)
def arrow(x1,y1,x2,y2,cls="flow",label=None,ly=None,lx=None,both=False):
    m=' marker-end="url(#ah)"'+(' marker-start="url(#ahs)"' if both else '')
    out=[f'<line class="{cls}" x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}"{m}/>']
    if label:
        lx = lx if lx is not None else (x1+x2)/2; ly = ly if ly is not None else (y1+y2)/2-6
        tw=len(label)*7.2+8
        out.append(f'<rect class="plate" x="{lx-tw/2}" y="{ly-11}" width="{tw}" height="15"/>')
        out.append(f'<text x="{lx}" y="{ly}" text-anchor="middle" class="al">{esc(label)}</text>')
    return "\n".join(out)
defs='''<defs><marker id="ah" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" class="head"/></marker>
<marker id="ahs" viewBox="0 0 10 10" refX="1" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M10,0 L0,5 L10,10 z" class="head"/></marker></defs>'''

# ---- diagram 1: architecture ----
H1=330; parts=[defs]
mw=340; relay_w=170; rx=(W-relay_w)/2
for side,(mx,name) in enumerate([(10,"MACHINE A"),(W-10-mw,"MACHINE B")]):
    parts.append(f'<rect class="zone" x="{mx}" y="10" width="{mw}" height="{H1-20}"/>')
    parts.append(f'<text x="{mx+10}" y="28" class="zl">{name}</text>')
    parts.append(f'<text x="{mx+mw-10}" y="28" text-anchor="end" class="zs">plaintext stays on the machine</text>')
    agents=[(["pi session"],"push"),(["Claude Code","Codex · Cursor"],"MCP"),(["shell agent"],"CLI")]
    AW, DW, GAP = 128, 96, 92
    ax = mx+12 if side==0 else mx+mw-12-AW
    dx = ax+AW+GAP if side==0 else mx+12
    if side==1: ax = dx+DW+GAP
    dy, dh = 60, 230
    parts.append(box(dx,dy,DW,dh,["daemon","keys","rules","spool"]))
    for i,(a,via) in enumerate(agents):
        ay=60+i*82
        parts.append(box(ax,ay,AW,56,a))
        cy=ay+28
        sock = "broker.sock" if i==0 else "daemon.sock"
        l, r = (ax+AW, dx) if side==0 else (dx+DW, ax)
        parts.append(arrow(l,cy,r,cy,"local",via,ly=cy-6,both=True))
        parts.append(f'<text x="{(l+r)/2}" y="{cy+17}" text-anchor="middle" class="zs">{sock}</text>')
# relay
parts.append(f'<rect class="relayzone" x="{rx}" y="70" width="{relay_w}" height="190"/>')
parts.append(f'<text x="{W/2}" y="92" text-anchor="middle" font-weight="800">letagents.talk relay</text>')
parts.append(f'<text x="{W/2}" y="110" text-anchor="middle" class="sub">one mailbox per device</text>')
parts.append(f'<rect class="hl" x="{W/2-76}" y="122" width="152" height="20"/>')
parts.append(f'<text x="{W/2}" y="136" text-anchor="middle" font-weight="800">sealed bytes only</text>')
parts.append(f'<text x="{W/2}" y="160" text-anchor="middle" class="sub">no sender identity</text>')
parts.append(f'<text x="{W/2}" y="176" text-anchor="middle" class="sub">no contacts or rules</text>')
parts.append(f'<text x="{W/2}" y="192" text-anchor="middle" class="sub">deleted on ack or after 48 h</text>')
daX=10+12+128+92+96; dbX=W-10-mw+12
parts.append(arrow(daX,120,rx,120,"sealed","POST",ly=112))
parts.append(arrow(rx+relay_w,120,dbX,120,"sealed","WSS",ly=112))
parts.append(arrow(dbX,230,rx+relay_w,230,"sealed","POST",ly=222))
parts.append(arrow(rx,230,daX,230,"sealed","WSS",ly=222))
svg1=f'<svg viewBox="0 0 {W} {H1}" role="img" aria-label="Architecture: agents on machine A talk to a local daemon over pi-intercom, MCP or the CLI; the daemon seals messages and sends them through the letagents.talk relay, which stores sealed bytes only, to the daemon on machine B, which delivers them to its agents.">\n'+"\n".join(parts)+'\n</svg>'

# ---- diagram 2: sequence of one ask ----
lanes=["agent A","daemon A","relay","daemon B","agent B"]
lx=[70+i*(W-140)/4 for i in range(5)]
steps=[(0,1,"ask (intercom or MCP)"),(1,1,"sign + seal"),(1,2,"POST /v1/send"),(2,3,"WSS push"),(3,3,"open · verify · rules"),
       (3,4,"push into session"),(4,3,"reply"),(3,2,"POST sealed reply"),(2,1,"WSS push"),(1,0,"reply closes the ask")]
top=46; dy=30; H2=top+len(steps)*dy+24
p=[defs]
for i,(n,x) in enumerate(zip(lanes,lx)):
    cls="node relayzone" if n=="relay" else "node"
    p.append(f'<rect class="{cls}" x="{x-58}" y="6" width="116" height="26"/>')
    p.append(f'<text x="{x}" y="23" text-anchor="middle" font-weight="800">{n}</text>')
    p.append(f'<line class="life" x1="{x}" y1="32" x2="{x}" y2="{H2-8}"/>')
for k,(a,b,label) in enumerate(steps):
    y=top+k*dy+10
    p.append(f'<text x="14" y="{y+4}" class="tick">{k+1:02d}</text>')
    if a==b:
        x=lx[a]; tw=len(label)*7.2+12
        p.append(f'<rect class="self" x="{x+6}" y="{y-11}" width="{tw}" height="18"/>')
        p.append(f'<text x="{x+12}" y="{y+2}" class="al">{esc(label)}</text>')
    else:
        cls = "sealed" if 2 in (a,b) else "local"
        p.append(arrow(lx[a],y,lx[b],y,cls,label,ly=y-5))
svg2=f'<svg viewBox="0 0 {W} {H2}" role="img" aria-label="Sequence of one ask: agent A asks, daemon A signs and seals, the relay pushes it to daemon B, which verifies and applies rules, pushes it into agent B\'s session; the reply travels back the same way and closes the ask.">\n'+"\n".join(p)+'\n</svg>'



# ---- diagram 3: one message in flight (CSS keyframes, no script) ----
T = 16  # seconds per loop
xs = [70 + i * (W - 140) / 4 for i in range(5)]
names = ["agent A", "daemon A", "relay", "daemon B", "agent B"]
states = ["plaintext", "sign + seal", "sealed bytes", "verify + open", "plaintext"]
variants = {
    "qp": ("are tests green?", "plain"),
    "qc": ("LA1 5f0e9b…c91a", "cipher"),
    "rp": ("yes, all green", "plain"),
    "rc": ("LA1 a71d03…4be2", "cipher"),
}
pts = [(0,0,"qp"),(6,0,"qp"),(14,1,"qp"),(18,1,"qc"),(25,2,"qc"),(29,2,"qc"),(36,3,"qc"),(40,3,"qp"),(46,4,"qp"),(51,4,"qp"),
       (53,4,"rp"),(57,4,"rp"),(64,3,"rp"),(68,3,"rc"),(75,2,"rc"),(79,2,"rc"),(86,1,"rc"),(90,1,"rp"),(96,0,"rp"),(100,0,"rp")]
H3 = 150; LY = 64; CW = 150
css = [f"@keyframes la-move {{" + " ".join(f"{p}% {{ transform: translate({xs[i]-CW/2:.1f}px, 0px); }}" for p,i,_ in pts) + "}"]
for v in variants:
    css.append(f"@keyframes la-{v} {{" + " ".join(f"{p}% {{ opacity: {1 if vv==v else 0}; }}" for p,_,vv in pts) + "}")
for s in range(5):
    css.append(f"@keyframes la-st{s} {{" + " ".join(f"{p}% {{ opacity: {1 if i==s else 0}; }}" for p,i,_ in pts) + "}")
css.append(f".la-chip {{ animation: la-move {T}s linear infinite; }}")
for v in variants:
    css.append(f".la-{v} {{ animation: la-{v} {T}s linear infinite; }}")
for s in range(5):
    css.append(f".la-st{s} {{ animation: la-st{s} {T}s linear infinite; }}")
css.append("@media (prefers-reduced-motion: reduce) { .la-chip, .la-chip * , [class^='la-st'] { animation: none !important; } .la-chip { display: none; } .la-static { display: inline; } }")
css.append(".la-static { display: none; }")
p3 = [f"<style>{' '.join(css)}</style>"]
p3.append(f'<line class="life" x1="{xs[0]}" y1="{LY}" x2="{xs[-1]}" y2="{LY}"/>')
p3.append(f'<line class="sealed" x1="{xs[1]}" y1="{LY}" x2="{xs[3]}" y2="{LY}"/>')
for s,(x,n,st) in enumerate(zip(xs,names,states)):
    cls = "node relayzone" if n == "relay" else "node"
    p3.append(f'<rect class="hl la-st{s}" x="{x-60}" y="6" width="120" height="26" style="opacity:0"/>')
    p3.append(f'<rect class="{cls}" x="{x-58}" y="8" width="116" height="22" style="fill-opacity:0"/>')
    p3.append(f'<text x="{x}" y="24" text-anchor="middle" font-weight="800">{n}</text>')
    p3.append(f'<circle cx="{x}" cy="{LY}" r="4" class="{"dot-s" if 1<=s<=3 else "dot"}"/>')
    p3.append(f'<text x="{x}" y="{LY+52}" text-anchor="middle" class="{"sub" if s in (0,4) else "zs"}">{st}</text>')
p3.append('<g class="la-chip">')
for v,(txt,kind) in variants.items():
    p3.append(f'<g class="la-{v}" style="opacity:0"><rect class="chip-{kind}" x="0" y="{LY-13}" width="{CW}" height="26"/>'
              f'<text x="{CW/2}" y="{LY+4}" text-anchor="middle" class="chip-{kind}-t">{esc(txt)}</text></g>')
p3.append('</g>')
p3.append(f'<text x="{W/2}" y="{LY+5}" text-anchor="middle" class="la-static zs">animation paused (reduced motion)</text>')
p3.append(f'<text x="{(xs[1]+xs[3])/2}" y="{H3-8}" text-anchor="middle" class="zs">only sealed bytes leave a machine · 1 KiB padded · the relay never holds a key</text>')
svg3 = f'<svg viewBox="0 0 {W} {H3}" role="img" aria-label="Animation: the question &quot;are tests green?&quot; leaves agent A as plaintext, daemon A signs and seals it into ciphertext, the relay carries sealed bytes, daemon B verifies and opens it back to plaintext for agent B, and the reply makes the same trip back.">\n' + "\n".join(p3) + "\n</svg>"
