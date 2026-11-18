# Generates the computed parts of web/index.html:
#   mark   the small logo: two lamps and a dotted arc between them (inline SVG)
#   path   the firefly path in the painting (web/art-night.webp), for the spark on the canvas layer
#   stars  the painted stars that twinkle on the canvas layer
#   lights the painted path lights and the lantern in the figure's hand, which glow when the pointer's lantern is near
# Coordinates of path and stars are in painting pixels (1024 x 1024), traced from the painting, and are
# written as fractions of the side so the canvas can scale them.
import json

SIDE = 1024

# The firefly path, from the envelope in the left figure's hand, past the closed post, up to the far window.
PATH = [
    (280, 820), (325, 802), (380, 782), (410, 780), (470, 770), (475, 745), (505, 725), (547, 712), (590, 704),
    (620, 695), (660, 687), (710, 660), (737, 650), (757, 640), (772, 627), (790, 610), (780, 597), (757, 585),
    (735, 575), (697, 560), (680, 547), (684, 534), (722, 515), (760, 502), (795, 492), (822, 480), (826, 466),
    (832, 452), (865, 435), (885, 423), (905, 407), (920, 393), (932, 383),
]
# Painted stars (x, y, brightness 0..1).
STARS = [
    (338, 113, 1), (669, 156, .7), (886, 136, .7), (114, 248, .6), (194, 238, .5), (612, 204, .45), (200, 141, .45),
    (438, 248, .4), (578, 178, .4), (528, 124, .35), (363, 272, .35), (627, 88, .3), (433, 368, .25), (308, 322, .25),
]
# The painted path lights (x, y, strength; 1 is the brightest path light), found as small warm local maxima near PATH,
# and the stronger lantern in the left figure's hand.
LIGHTS = [
    (300, 806, .58), (328, 801, 1), (350, 792, .92), (371, 794, .83), (381, 783, .72), (409, 781, 1), (442, 779, .78),
    (470, 743, .77), (472, 770, .94), (482, 757, .56), (504, 726, 1), (547, 711, .91), (589, 703, .71), (626, 695, .93),
    (658, 688, .79), (676, 546, .58), (679, 538, .82), (694, 677, .76), (697, 560, .68), (701, 527, .94), (711, 660, 1),
    (717, 517, .7), (717, 569, .92), (724, 516, .47), (737, 651, .57), (757, 584, .84), (757, 641, 1), (758, 503, .89),
    (772, 499, .66), (772, 631, .62), (780, 597, .8), (787, 621, .73), (796, 494, 1), (817, 460, 1), (822, 484, .57),
    (832, 449, .43), (848, 442, .96), (872, 429, .65), (895, 411, .69), (906, 403, .43), (232, 843, 1.6),
]
# The two lit windows: the sender's (near, left) and the receiver's (far, right).
WINDOWS = [(168, 780, 34), (932, 383, 12)]


def frac(v):
    return round(v / SIDE, 4)


path = json.dumps([[frac(x), frac(y)] for x, y in PATH], separators=(",", ":"))
stars = json.dumps([[frac(x), frac(y), b] for x, y, b in STARS], separators=(",", ":"))
lights = json.dumps([[frac(x), frac(y), b] for x, y, b in LIGHTS], separators=(",", ":"))
windows = json.dumps([[frac(x), frac(y), frac(r)] for x, y, r in WINDOWS], separators=(",", ":"))

# Mark: two lamps on a 22 x 22 grid, joined by dots along a quadratic arc that sags between them.
A, C, B = (3.5, 15.5), (11, 20), (18.5, 6.5)
dots = []
for i in range(1, 6):
    t = i / 6
    x = (1 - t) ** 2 * A[0] + 2 * (1 - t) * t * C[0] + t ** 2 * B[0]
    y = (1 - t) ** 2 * A[1] + 2 * (1 - t) * t * C[1] + t ** 2 * B[1]
    dots.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r=".9"/>')
mark = ('<svg class="mark" viewBox="0 0 22 22" width="22" height="22" aria-hidden="true" focusable="false">'
        f'<g fill="currentColor" opacity=".55">{"".join(dots)}</g>'
        f'<circle cx="{A[0]}" cy="{A[1]}" r="2.4" fill="var(--amber)"/><circle cx="{B[0]}" cy="{B[1]}" r="2.4" fill="var(--amber)"/></svg>')
