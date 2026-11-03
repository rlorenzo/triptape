#!/bin/sh
# Regenerate resources/gazetteer/: cities.tsv (GeoNames, CC BY 4.0),
# attractions.tsv (Wikidata, CC0), land-grid.bin (Natural Earth, public domain).
set -eu
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sO --max-time 300 --output-dir "$tmp" https://download.geonames.org/export/dump/cities15000.zip
curl -s --max-time 120 -o "$tmp/countryInfo.txt" https://download.geonames.org/export/dump/countryInfo.txt
unzip -o -q "$tmp/cities15000.zip" -d "$tmp"
python3 - "$tmp" <<'PY'
import sys
tmp = sys.argv[1]
countries = {}
with open(f'{tmp}/countryInfo.txt', encoding='utf-8') as f:
    for line in f:
        if line.startswith('#') or not line.strip():
            continue
        p = line.rstrip('\n').split('\t')
        if len(p) > 4:
            countries[p[0]] = p[4]
rows = []
with open(f'{tmp}/cities15000.txt', encoding='utf-8') as f:
    for line in f:
        p = line.rstrip('\n').split('\t')
        try:
            pop = int(p[14])
        except (ValueError, IndexError):
            continue
        if pop < 15000:
            continue
        rows.append((p[2] or p[1], countries.get(p[8], p[8]), p[4], p[5]))
rows.sort(key=lambda r: (r[1], r[0]))
with open('resources/gazetteer/cities.tsv', 'w', encoding='utf-8') as f:
    for r in rows:
        f.write('\t'.join(r) + '\n')
print(f'cities kept: {len(rows)}')
PY

# --- attractions.tsv from Wikidata (CC0) -------------------------------------
# Tourist attractions plus named place classes, including subclasses
# (P31/P279*); >=12 sitelinks as a notability floor. One query per class
# against the public QLever Wikidata endpoint (WDQS rate-limits and
# times out on these). Output: name, lat, lon, most notable first.
# Any failed or malformed query aborts before attractions.tsv is touched.
# The tourist-attraction class also holds whole towns (spa towns, resorts),
# which must keep their `City, Country` label: a row reached only through
# that class is dropped when it is also typed as a city, town, village,
# municipality, or spa town, unless it is an archaeological site (Persepolis).
UA='TripTape-gazetteer-build/1.0'
MIN_LINKS=12
# class:floor. Each floor is about 90% of that class's row count from the
# recorded full run on 2026-11-03 (QLever). This catches a response cut off
# at a row boundary, which still parses cleanly. Reset these from a recorded
# run whenever the class list or endpoint changes; never lower them to make
# a partial run pass.
CLASS_FLOORS='Q570116:1400 Q194195:107 Q22746:108 Q22698:2355 Q33506:3306 Q167346:94 Q43501:144 Q23413:821'
# Same floor rule. These filter rows out, so a cut-off response here would
# silently let towns back in.
TOWN_FLOORS='Q515:12900 Q3957:16004 Q532:34452 Q15284:120942 Q4946461:311'
SITE_FLOORS='Q839954:2390'
for entry in $CLASS_FLOORS; do
  cls=${entry%:*}
  q="PREFIX wd: <http://www.wikidata.org/entity/> PREFIX wdt: <http://www.wikidata.org/prop/direct/> PREFIX wikibase: <http://wikiba.se/ontology#> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> SELECT ?item ?itemLabel ?coord ?links WHERE { ?item wdt:P31/wdt:P279* wd:$cls; wdt:P625 ?coord; wikibase:sitelinks ?links. FILTER(?links >= $MIN_LINKS) ?item rdfs:label ?itemLabel. FILTER(LANG(?itemLabel) = 'en') }"
  curl -fsS --retry 3 --max-time 600 -H "User-Agent: $UA" -H 'Accept: text/csv' \
    --data-urlencode "query=$q" 'https://qlever.dev/api/wikidata' > "$tmp/wd-$cls.csv"
done
for entry in $TOWN_FLOORS $SITE_FLOORS; do
  cls=${entry%:*}
  q="PREFIX wd: <http://www.wikidata.org/entity/> PREFIX wdt: <http://www.wikidata.org/prop/direct/> PREFIX wikibase: <http://wikiba.se/ontology#> SELECT DISTINCT ?item WHERE { ?item wdt:P31/wdt:P279* wd:$cls; wdt:P625 ?coord; wikibase:sitelinks ?links. FILTER(?links >= $MIN_LINKS) }"
  curl -fsS --retry 3 --max-time 600 -H "User-Agent: $UA" -H 'Accept: text/csv' \
    --data-urlencode "query=$q" 'https://qlever.dev/api/wikidata' > "$tmp/members-$cls.csv"
done
# shellcheck disable=SC2086 # Each floor list splits into one argument per class.
python3 - "$tmp" "$MIN_LINKS" "$TOWN_FLOORS" "$SITE_FLOORS" $CLASS_FLOORS <<'PY'
import csv, re, sys
tmp, min_links = sys.argv[1], int(sys.argv[2])
class_floors = [arg.split(':') for arg in sys.argv[5:]]
MIN_TOTAL_ROWS = 5195
BROAD_CLASS = 'Q570116'

def members(floors):
    # Every row must be an item URL and every class must clear its floor,
    # for the same reasons as the class queries below.
    found = set()
    for cls, floor in (entry.split(':') for entry in floors.split()):
        with open(f'{tmp}/members-{cls}.csv', encoding='utf-8', newline='') as fh:
            reader = csv.reader(fh)
            if next(reader, None) != ['item']:
                sys.exit(f'{cls}: response is not the expected CSV')
            count = 0
            for row in reader:
                if len(row) != 1 or not re.fullmatch(r'\S+/Q\d+', row[0]):
                    sys.exit(f'{cls}: malformed row {reader.line_num}: {row!r}')
                count += 1
                found.add(row[0].rsplit('/', 1)[-1])
        print(f'{cls}: {count} members')
        if count < int(floor):
            sys.exit(f'{cls}: only {count} members, expected at least {floor}')
    return found

towns, sites = members(sys.argv[3]), members(sys.argv[4])
rows = {}
reached_via = {}
for cls, floor in class_floors:
    # The endpoint can answer 200 and then cut the stream off on a server
    # timeout, so every row must parse: a short or garbled row is fatal.
    with open(f'{tmp}/wd-{cls}.csv', encoding='utf-8', newline='') as fh:
        reader = csv.reader(fh)
        if next(reader, None) != ['item', 'itemLabel', 'coord', 'links']:
            sys.exit(f'{cls}: response is not the expected CSV')
        count = 0
        for row in reader:
            if len(row) != 4 or not row[3].isdigit():
                sys.exit(f'{cls}: malformed row {reader.line_num}: {row!r}')
            links = int(row[3])
            if links < min_links:
                sys.exit(f'{cls}: row {reader.line_num} has {links} sitelinks')
            count += 1
            qid = row[0].rsplit('/', 1)[-1]
            m = re.match(r'(?i)point\(([-\d.]+) ([-\d.]+)\)', row[2])
            # Non-point coordinates can't be looked up, and an unlabeled
            # item comes back with its QID as the label: skip, not fatal.
            if not m or not row[1] or row[1] == qid:
                continue
            lon, lat = float(m.group(1)), float(m.group(2))
            # The endpoint's row order varies and an item can have several
            # coordinates or labels: pick one the same way on every run.
            candidate = (row[1], lat, lon, links)
            if qid not in rows or candidate < rows[qid]:
                rows[qid] = candidate
            reached_via.setdefault(qid, set()).add(cls)
    print(f'{cls}: {count} rows')
    if count < int(floor):
        sys.exit(f'{cls}: only {count} rows, expected at least {floor}')
for qid, classes in reached_via.items():
    if classes == {BROAD_CLASS} and qid in towns and qid not in sites:
        del rows[qid]
if len(rows) < MIN_TOTAL_ROWS:
    sys.exit(f'only {len(rows)} attractions, expected at least {MIN_TOTAL_ROWS}')
# Most notable first: lookups keep the first of equidistant items.
out = sorted(rows.values(), key=lambda r: (-r[3], r[0], r[1], r[2]))
with open('resources/gazetteer/attractions.tsv', 'w', encoding='utf-8') as f:
    for name, lat, lon, _ in out:
        f.write(f'{name}\t{lat}\t{lon}\n')
print(f'attractions kept: {len(out)}')
PY

# --- land-grid.bin from Natural Earth 50m (public domain) --------------------
# 0.05-degree land bitmap, 7200x3600 bits, row-major from (-90, -180).
curl -sL --max-time 300 -o "$tmp/ne_50m_land.geojson" \
  https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_50m_land.geojson
python3 - "$tmp" <<'PY'
import json, sys
tmp = sys.argv[1]
data = json.load(open(f'{tmp}/ne_50m_land.geojson'))
RES = 20  # Cells per degree; matches CELLS_PER_DEG in geo.rs.
W, H = 360 * RES, 180 * RES
grid = bytearray(W * H // 8)
polys = []
for f in data['features']:
    g = f['geometry']
    if g['type'] == 'Polygon':
        polys.append(g['coordinates'])
    elif g['type'] == 'MultiPolygon':
        polys.extend(g['coordinates'])
# Edge crossings per row; each edge only visits the rows it can span.
crossings = [[] for _ in range(H)]
for rings in polys:
    for ring in rings:
        for (x1, y1), (x2, y2) in zip(ring, ring[1:]):
            r0 = int((min(y1, y2) + 90) * RES)
            r1 = int((max(y1, y2) + 90) * RES)
            for r in range(r0, min(H - 1, r1) + 1):
                lat = -90 + (r + 0.5) * (1 / RES)
                if (y1 <= lat < y2) or (y2 <= lat < y1):
                    crossings[r].append(x1 + (lat - y1) * (x2 - x1) / (y2 - y1))
for r, xs in enumerate(crossings):
    xs.sort()
    for i in range(0, len(xs) - 1, 2):
        c0 = int((xs[i] + 180) * RES)
        c1 = int((xs[i + 1] + 180) * RES)
        for c in range(max(0, c0), min(W - 1, c1) + 1):
            bit = r * W + c
            grid[bit // 8] |= 1 << (bit % 8)
open('resources/gazetteer/land-grid.bin', 'wb').write(grid)
print(f'land grid bytes: {len(grid)}')
PY
