#!/usr/bin/env bash
# Book spike (Sprint 0): Typst 4-page proof PDF + per-page JPEGs at exact
# pixel dimensions (10x8in @ 300 PPI = 3000x2400). Asserts dims via ffprobe.
# Outputs: /tmp/book-spike/proof.pdf, /tmp/book-spike/page-N.jpg
set -euo pipefail
OUT="/tmp/book-spike"
rm -rf "$OUT"; mkdir -p "$OUT"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

typst compile "$ROOT/fixtures/book-spike.typ" "$OUT/proof.pdf"
typst compile --format png --ppi 300 "$ROOT/fixtures/book-spike.typ" "$OUT/page-{p}.png"

[ -s "$OUT/proof.pdf" ] || { echo "FAIL: proof.pdf missing/empty"; exit 1; }
PAGES="$(python3 -c "import re;d=open('$OUT/proof.pdf','rb').read();print(len(re.findall(rb'/Type\s*/Page[^s]',d)))")"
echo "proof.pdf pages (approx): $PAGES"
for i in 1 2 3 4; do
  ffmpeg -hide_banner -v error -y -i "$OUT/page-$i.png" -q:v 2 "$OUT/page-$i.jpg"
  DIMS="$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$OUT/page-$i.jpg")"
  [ "$DIMS" = "3000,2400" ] || { echo "FAIL: page-$i.jpg dims $DIMS, want 3000,2400"; exit 1; }
  echo "ok: page-$i.jpg $DIMS"
done
echo "PASS: proof.pdf + 4 JPEGs at 3000x2400 in $OUT"
