# Builds web/index.html from page.html and the parts computed by gen.py (logo mark, firefly path, stars, lights),
# and web/404.html (the page browsers get for an unknown path) from 404.html.
import pathlib, runpy
here = pathlib.Path(__file__).parent
g = runpy.run_path(str(here / "gen.py"), run_name="gen")
page = (here / "page.html").read_text()
for key in ("mark", "path", "stars", "lights", "windows"):
    page = page.replace(f"__{key.upper()}__", g[key])
(here.parent / "index.html").write_text(page)
(here.parent / "404.html").write_text((here / "404.html").read_text().replace("__MARK__", g["mark"]))
print("wrote web/index.html, web/404.html")
