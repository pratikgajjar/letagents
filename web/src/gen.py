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
parts.append(f'<text x="{W/2}" y="176" text-anchor="middle" class="sub">no contacts, no rules</text>')
parts.append(f'<text x="{W/2}" y="192" text-anchor="middle" class="sub">deleted on ack or 48 h</text>')
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


