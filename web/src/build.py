# Builds web/index.html from page.html and the diagram computed by gen.py.
import pathlib, runpy
here = pathlib.Path(__file__).parent
g = runpy.run_path(str(here / "gen.py"), run_name="gen")
page = (here / "page.html").read_text()
(here.parent / "index.html").write_text(page.replace("__SVG__", g["svg"]))
print("wrote web/index.html")
