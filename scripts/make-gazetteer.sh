#!/bin/sh
# Regenerate resources/gazetteer/cities.tsv from GeoNames (CC BY 4.0).
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
