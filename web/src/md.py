# A small markdown renderer for the docs pages (web/agents.html, web/protocol.html, web/skill.html).
# It covers what the sources use: YAML front matter, ATX headings, paragraphs, indented and fenced code,
# "-" and "1." lists (one level, with continuation lines), pipe tables and "---" rules; inline code,
# **bold**, *em*, [links](url) and bare https:// links. Every piece of source text is HTML-escaped.
# Deterministic: the same input always gives the same bytes.
import html
import re

SITE = "https://letagents.talk/"


def esc(s):
    # Text: & < > escaped. Attribute values go through attr().
    return html.escape(s, quote=False)


def attr(s):
    return html.escape(s, quote=True)


SAFE_URL = re.compile(r"^(https?://|mailto:|/|#|\?)", re.I)


def href(url):
    # Links to this site stay on the site that serves the page (also on a local relay).
    if url.startswith(SITE):
        url = "/" + url[len(SITE):]
    return attr(url)


INLINE = re.compile(
    r"(?P<code>`+)(?P<ctext>.+?)(?P=code)"
    r"|\*\*(?P<bold>[^*\s](?:.*?[^*\s])?)\*\*"
    r"|(?<![\w*])\*(?P<em>[^*\s](?:[^*]*?[^*\s])?)\*(?![\w*])"
    r"|\[(?P<ltext>[^\]]+)\]\((?P<lurl>[^)\s]+)\)"
    r"|(?P<url>https?://[^\s<>()`]+[^\s<>()`.,;:!?'\"])"
)


def inline(s):
    out, pos = [], 0
    for m in INLINE.finditer(s):
        out.append(esc(s[pos:m.start()]))
        pos = m.end()
        if m["code"]:
            out.append(f"<code>{esc(m['ctext'])}</code>")
        elif m["bold"]:
            out.append(f"<strong>{inline(m['bold'])}</strong>")
        elif m["em"]:
            out.append(f"<em>{inline(m['em'])}</em>")
        elif m["ltext"] and SAFE_URL.match(m["lurl"]):
            out.append(f'<a href="{href(m["lurl"])}">{inline(m["ltext"])}</a>')
        elif m["ltext"]:
            out.append(esc(m[0]))  # javascript: and other schemes stay text
        else:
            out.append(f'<a href="{href(m["url"])}">{esc(m["url"])}</a>')
    out.append(esc(s[pos:]))
    return "".join(out)


def plain(s):
    # Heading text without markup, for ids and the table of contents.
    return re.sub(r"`+|\*\*|\*", "", s)


def slug(text, seen):
    base = re.sub(r"[^a-z0-9]+", "-", plain(text).lower()).strip("-") or "section"
    s, n = base, 2
    while s in seen:
        s, n = f"{base}-{n}", n + 1
    seen.add(s)
    return s


def cells(row):
    # Split a table row on | outside code spans.
    row = row.strip()
    if row.startswith("|"):
        row = row[1:]
    if row.endswith("|"):
        row = row[:-1]
    out, cur, tick = [], "", False
    for ch in row:
        if ch == "`":
            tick = not tick
        if ch == "|" and not tick:
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    out.append(cur.strip())
    return out


LIST = re.compile(r"^(?P<ind> {0,3})(?P<mark>[-*+]|\d{1,9}[.)])\s+(?P<text>.*)$")
HEAD = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE = re.compile(r"^(```+|~~~+)\s*([\w+-]*)\s*$")
RULE = re.compile(r"^ {0,3}([-*_])( *\1){2,}\s*$")
DELIM = re.compile(r"^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$")


COMMENT = re.compile(r"^(.*?\S)?(\s{2,}#\s.*|#\s.*)$")


def code_line(ln):
    # A shell comment at the end of a line (two spaces, then "# ") is drawn quieter.
    m = COMMENT.match(ln)
    if not m:
        return esc(ln)
    return esc(m[1] or "") + f'<span class="c">{esc(m[2])}</span>'


def code_block(text, lang=""):
    cls = f' class="language-{attr(lang)}"' if lang else ""
    body = "\n".join(code_line(ln) for ln in text.split("\n")) if lang in ("", "sh", "bash", "shell") else esc(text)
    return f'<div class="code"><pre tabindex="0"><code{cls}>{body}</code></pre></div>'


def render(src):
    """Returns (front matter dict, first h1 as markdown, body html without that h1, toc list of (level, id, text html))."""
    lines = src.replace("\r\n", "\n").split("\n")
    front = {}
    if lines and lines[0] == "---":
        end = lines.index("---", 1)
        for ln in lines[1:end]:
            k, _, v = ln.partition(":")
            front[k.strip()] = v.strip()
        lines = lines[end + 1:]
    out, toc, seen, title = [], [], set(), ""
    i, n = 0, len(lines)
    blank = lambda s: not s.strip()
    while i < n:
        ln = lines[i]
        if blank(ln):
            i += 1
            continue
        m = HEAD.match(ln)
        if m:
            level, text = len(m[1]), m[2]
            if level == 1 and not title:
                title = text
            else:
                hid = slug(text, seen)
                out.append(f'<h{level} id="{hid}">{inline(text)}<a class="anchor" href="#{hid}" aria-label="Link to this section">#</a></h{level}>')
                if level in (2, 3):
                    toc.append((level, hid, inline(text)))
            i += 1
            continue
        m = FENCE.match(ln)
        if m:
            j, body = i + 1, []
            while j < n and not lines[j].startswith(m[1]):
                body.append(lines[j])
                j += 1
            out.append(code_block("\n".join(body), m[2]))
            i = j + 1
            continue
        if ln.startswith("    ") or ln.startswith("\t"):
            body = []
            while i < n and (lines[i].startswith("    ") or lines[i].startswith("\t") or blank(lines[i])):
                body.append(lines[i][4:] if lines[i].startswith("    ") else lines[i][1:] if lines[i].startswith("\t") else "")
                i += 1
            while body and not body[-1].strip():
                body.pop()
            out.append(code_block("\n".join(body)))
            continue
        if RULE.match(ln):
            out.append("<hr>")
            i += 1
            continue
        if "|" in ln and i + 1 < n and DELIM.match(lines[i + 1]):
            head = cells(ln)
            aligns = []
            for c in cells(lines[i + 1]):
                aligns.append("right" if c.endswith(":") and not c.startswith(":") else "center" if c.startswith(":") and c.endswith(":") else "")
            i += 2
            rows = []
            while i < n and "|" in lines[i] and not blank(lines[i]):
                rows.append(cells(lines[i]))
                i += 1
            al = lambda k: f' style="text-align:{aligns[k]}"' if k < len(aligns) and aligns[k] else ""
            th = "".join(f"<th{al(k)}>{inline(c)}</th>" for k, c in enumerate(head))
            # data-label: the column name, shown beside each cell when a narrow screen stacks the rows.
            lab = [attr(plain(c)) for c in head]
            td = lambda k, c: f'<td data-label="{lab[k]}"{al(k)}>' + (f"<span>{inline(c)}</span>" if c else "") + "</td>"
            trs = "".join("<tr>" + "".join(td(k, r[k] if k < len(r) else "") for k in range(len(head))) + "</tr>" for r in rows)
            out.append(f'<div class="table" tabindex="0" role="region" aria-label="Table"><table><thead><tr>{th}</tr></thead><tbody>{trs}</tbody></table></div>')
            continue
        m = LIST.match(ln)
        if m:
            ordered = m["mark"][0].isdigit()
            start = int(m["mark"][:-1]) if ordered else 1
            items = []
            while i < n:
                m = LIST.match(lines[i])
                if not m or m["mark"][0].isdigit() != ordered:
                    break
                item = [m["text"]]
                i += 1
                # Continuation lines: indented, or lazy (not a new block).
                while i < n and not blank(lines[i]) and not LIST.match(lines[i]) and not HEAD.match(lines[i]):
                    item.append(lines[i].strip())
                    i += 1
                items.append(item)
                if i < n and blank(lines[i]) and i + 1 < n and LIST.match(lines[i + 1]):
                    i += 1  # a loose list: a blank line between items
            lis = "".join("<li>" + "\n".join(inline(t) for t in it) + "</li>" for it in items)
            if ordered:
                out.append(f'<ol{"" if start == 1 else f" start={start}"}>{lis}</ol>')
            else:
                out.append(f"<ul>{lis}</ul>")
            continue
        if ln.startswith(">"):
            body = []
            while i < n and lines[i].startswith(">"):
                body.append(lines[i][1:].lstrip())
                i += 1
            out.append(f"<blockquote><p>{chr(10).join(inline(t) for t in body)}</p></blockquote>")
            continue
        para = []
        while i < n and not blank(lines[i]) and not HEAD.match(lines[i]) and not FENCE.match(lines[i]) and not LIST.match(lines[i]) \
                and not lines[i].startswith(">") and not (para and "|" in lines[i] and i + 1 < n and DELIM.match(lines[i + 1])):
            para.append(lines[i].strip())
            i += 1
        out.append(f"<p>{chr(10).join(inline(t) for t in para)}</p>")
    return front, title, "\n".join(out), toc


def selftest():
    """Escaping checks, run by build.py before it writes anything."""
    bad = '<script>alert(1)</script> & "q" <img src=x onerror=alert(1)>'
    _, _, body, _ = render(f"## {bad}\n\n{bad} [x](javascript:alert(1)) `{bad}` **{bad}**\n\n| {bad} |\n|---|\n| {bad} |\n\n- {bad}\n\n    {bad}  # {bad}\n")
    for tag in ("<script", "<img", "<b>", 'href="javascript'):
        assert tag not in body.replace('<a class="anchor"', ""), (tag, body)
    assert inline('https://e.com/?a=1&b=2') == '<a href="https://e.com/?a=1&amp;b=2">https://e.com/?a=1&amp;b=2</a>'
    assert inline("[a](/x?y=\"z)") .count('"') == 2, inline("[a](/x?y=\"z)")
