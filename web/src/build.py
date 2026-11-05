# Builds web/index.html from page.html and the diagrams computed by gen.py.
import pathlib, runpy
here = pathlib.Path(__file__).parent
g = runpy.run_path(str(here / "gen.py"), run_name="gen")
page = (here / "page.html").read_text()
(here.parent / "index.html").write_text(page.replace("__SVG1__", g["svg1"]).replace("__SVG2__", g["svg2"]).replace("__SVG3__", g["svg3"]))
print("wrote web/index.html")
