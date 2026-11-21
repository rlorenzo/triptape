#!/bin/sh
# Regenerates resources/gazetteer/coast.bin: the world's coastlines as
# polygon rings, for the parchment travel maps (Natural Earth 10m land,
# public domain). Run from the repo root. Needs curl and python3.
#
# Format (little-endian): "TTC1", u32 ring count, then per ring
#   u32 n, i32 lon0, i32 lat0, (n - 1) x (i16 dlon, i16 dlat)
# in units of 0.001 degree. Rings are closed (first point not repeated).
set -eu
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
src="${1:-}"
if [ -z "$src" ]; then
  src="$tmp/ne_10m_land.geojson"
  curl -fsSL --retry 3 --max-time 600 -o "$src" \
    https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_10m_land.geojson
fi
python3 - "$src" <<'PY'
import json, struct, sys

TOL = 0.004   # degrees (~400 m): finer than a pixel on any map we draw
UNIT = 1000   # stored units per degree
STEP = 30000  # largest stored delta (i16)


def simplify(pts, tol):
    """Douglas-Peucker on an open run of points; keeps both ends."""
    n = len(pts)
    keep = [False] * n
    keep[0] = keep[-1] = True
    stack = [(0, n - 1)]
    while stack:
        a, b = stack.pop()
        (x1, y1), (x2, y2) = pts[a], pts[b]
        dx, dy = x2 - x1, y2 - y1
        length = dx * dx + dy * dy
        best, at = 0.0, -1
        for i in range(a + 1, b):
            px, py = pts[i]
            t = 0.0 if length == 0 else max(0.0, min(1.0, ((px - x1) * dx + (py - y1) * dy) / length))
            d = (px - x1 - t * dx) ** 2 + (py - y1 - t * dy) ** 2
            if d > best:
                best, at = d, i
        if best > tol * tol:
            keep[at] = True
            stack += [(a, at), (at, b)]
    return [p for p, k in zip(pts, keep) if k]


data = json.load(open(sys.argv[1]))
rings = []
for f in data['features']:
    g = f['geometry']
    polys = [g['coordinates']] if g['type'] == 'Polygon' else g['coordinates']
    for poly in polys:
        rings.extend(poly)

out = bytearray(b'TTC1')
body = bytearray()
kept = points = 0
for ring in rings:
    pts = simplify([(x, y) for x, y in ring], TOL)  # GeoJSON rings repeat the first point
    q = []
    for x, y in pts[:-1]:
        p = (round(x * UNIT), round(y * UNIT))
        if not q or q[-1] != p:
            q.append(p)
    if len(q) < 3:
        continue
    # A long straight edge (Antarctica's base) is split to fit an i16 delta.
    full = [q[0]]
    for p in q[1:]:
        a = full[-1]
        steps = max(1, -(-max(abs(p[0] - a[0]), abs(p[1] - a[1])) // STEP))
        for s in range(1, steps + 1):
            full.append((a[0] + (p[0] - a[0]) * s // steps, a[1] + (p[1] - a[1]) * s // steps))
    body += struct.pack('<Iii', len(full), *full[0])
    for a, b in zip(full, full[1:]):
        body += struct.pack('<hh', b[0] - a[0], b[1] - a[1])
    kept += 1
    points += len(full)
out += struct.pack('<I', kept) + body
open('resources/gazetteer/coast.bin', 'wb').write(out)
print(f'coast rings: {kept}, points: {points}, bytes: {len(out)}')
PY
