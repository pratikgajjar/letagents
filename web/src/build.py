# Builds the generated pages in web/ (run from anywhere: python3 web/src/build.py):
#   web/index.html   the landing page, from page.html and the parts computed by gen.py (logo mark, firefly path, stars, lights)
#   web/404.html     the page browsers get for an unknown path, from 404.html
#   web/agents.html, web/protocol.html, web/skill.html
#                    what browsers get for /agents.md, /protocol.md and /skill.md: the same markdown sources
#                    (web/agents.md, PROTOCOL.md, skills/letagents/SKILL.md), rendered by md.py into doc.html.
#                    Agents and other clients get the markdown itself (see relay/src/lib.rs, wants_html_page).
# Standard library only; the output is the same bytes for the same input. CI fails if a committed file is stale.
import pathlib, re, runpy, sys

here = pathlib.Path(__file__).parent
web, root = here.parent, here.parent.parent
sys.dont_write_bytecode = True  # no __pycache__ in web/src
sys.path.insert(0, str(here))
import md  # noqa: E402

md.selftest()

g = runpy.run_path(str(here / "gen.py"), run_name="gen")
page = (here / "page.html").read_text()
for key in ("mark", "path", "stars", "lights", "windows"):
    page = page.replace(f"__{key.upper()}__", g[key])
(web / "index.html").write_text(page)
(web / "404.html").write_text((here / "404.html").read_text().replace("__MARK__", g["mark"]))

# path, source, eyebrow, title, description (None: the source's front matter description)
DOCS = [
    ("/agents.md", "web/agents.md", "Guide · for agents and their humans", "Setup guide for AI agents · letagents",
     "Connect AI agents on two machines with one line each: install, invite, addresses, MCP and pi-intercom, guest agents, and the rules agents follow."),
    ("/protocol.md", "PROTOCOL.md", "Protocol · wire format v1", "Protocol v1 · letagents",
     "The letagents wire format: device keys, sealing with X25519, HKDF and AES-256-GCM, drop codes, the relay API, and what the relay keeps."),
    ("/skill.md", "skills/letagents/SKILL.md", "Skill · for your agent's skills folder", "Agent skill · letagents", None),
]
NAV = [("/agents.md", "Guide"), ("/protocol.md", "Protocol"), ("https://github.com/pratikgajjar/letagents", "GitHub")]


def fill(template, values):
    # One pass, so text put in cannot be read as another placeholder.
    return re.sub(r"__([A-Z0-9]+)__", lambda m: values[m[1]] if m[1] in values else m[0], template)


doc = (here / "doc.html").read_text()
for k, (path, src, eyebrow, title, desc) in enumerate(DOCS):
    front, h1, body, toc = md.render((root / src).read_text())
    desc = desc or front.get("description", "")
    items = "".join(f'<li class="l{lv}"><a href="#{hid}">{text}</a></li>' for lv, hid, text in toc)
    front_html = ""
    if front:
        rows = "".join(f"<dt>{md.esc(key)}</dt><dd>{md.inline(val)}</dd>" for key, val in front.items())
        front_html = f'\n    <dl class="front" aria-label="Skill front matter">{rows}</dl>'
    here_ = ' aria-current="page"'
    nav = "".join(f'<a href="{u}"{here_ if u == path else ""}>{t}</a>' for u, t in NAV)
    prev_, next_ = DOCS[k - 1] if k else None, DOCS[k + 1] if k + 1 < len(DOCS) else None
    nxt = ""
    if prev_:
        nxt += f'<a href="{prev_[0]}"><span>Previous</span><b>{md.esc(prev_[3].split(" · ")[0])}</b></a>'
    if next_:
        nxt += f'<a href="{next_[0]}"><span>Next</span><b>{md.esc(next_[3].split(" · ")[0])}</b></a>'
    out = fill(doc, {
        "TITLE": md.attr(title), "DESC": md.attr(desc), "PATH": path, "FILE": md.esc(path[1:]), "SRC": src,
        "EYEBROW": md.esc(eyebrow), "H1": md.inline(h1), "FRONT": front_html, "MARK": g["mark"], "NAV": nav,
        "TOC": items, "TOCM": items, "BODY": body, "NEXT": nxt,
    })
    (web / f"{path[1:-3]}.html").write_text(out)

print("wrote web/index.html, web/404.html, web/agents.html, web/protocol.html, web/skill.html")
