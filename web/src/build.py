# Builds web/index.html from page.html and the parts computed by gen.py (logo mark, firefly path, stars).
import pathlib, runpy
here = pathlib.Path(__file__).parent
g = runpy.run_path(str(here / "gen.py"), run_name="gen")
page = (here / "page.html").read_text()
for key in ("mark", "path", "stars", "windows"):
    page = page.replace(f"__{key.upper()}__", g[key])
(here.parent / "index.html").write_text(page)
print("wrote web/index.html")
