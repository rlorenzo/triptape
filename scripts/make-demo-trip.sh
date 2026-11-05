#!/usr/bin/env bash
# Build a realistic-looking demo trip: 39 Pexels photos of one family, stamped
# with STAGED capture dates and GPS so TripTape can group them by day and place.
#
# The photos are one beach shoot by Kampus Production; Pexels serves them with
# no date or location. The three-day itinerary below is invented for the demo.
# Pexels restricts redistribution, so this fetches them on demand: never commit
# the output. License: https://www.pexels.com/license/
#
# Usage: ./scripts/make-demo-trip.sh [out-dir]   (default: /tmp/demo-family-trip)
set -euo pipefail

OUT="${1:-/tmp/demo-family-trip}"
TZ_OFFSET="+01:00" # Portugal, summer
STEP_MIN=17        # Under the 4 h gap that would split a day into two chapters.

command -v exiftool >/dev/null || { echo "exiftool is required" >&2; exit 1; }
mkdir -p "$OUT"

total=0

# stamp_day <YYYY:MM:DD> <start-hour> <lat> <lon-west> <pexels-id>...
stamp_day() {
  local day="$1" hour="$2" lat="$3" lon="$4" i=0 id dest mins when plat plon
  shift 4
  for id in "$@"; do
    dest="$OUT/IMG_$id.jpg"
    if [ ! -s "$dest" ]; then
      curl -fsSL --retry 2 -m 120 -A "Mozilla/5.0" -o "$dest.part" \
        "https://images.pexels.com/photos/$id/pexels-photo-$id.jpeg"
      mv "$dest.part" "$dest"
      sleep 0.25
    fi
    mins=$((hour * 60 + i * STEP_MIN))
    when=$(printf '%s %02d:%02d:%02d' "$day" $((mins / 60)) $((mins % 60)) $(((i * 37) % 60)))
    plat=$(awk -v a="$lat" -v i="$i" 'BEGIN { printf "%.5f", a + (i % 5) * 0.00012 }')
    plon=$(awk -v a="$lon" -v i="$i" 'BEGIN { printf "%.5f", a + (i % 3) * 0.00012 }')
    # TripTape signs GPS from the *Ref tags, so the longitude is stored unsigned.
    # Don't write a description or caption tag: TripTape shows those as captions.
    exiftool -q -overwrite_original \
      -DateTimeOriginal="$when" -CreateDate="$when" \
      -OffsetTimeOriginal="$TZ_OFFSET" \
      -GPSLatitude="$plat" -GPSLatitudeRef=N \
      -GPSLongitude="$plon" -GPSLongitudeRef=W \
      -XMP-dc:Source="Photo: Kampus Production on Pexels (ID $id). Capture date and GPS are staged." \
      -FileModifyDate="$when$TZ_OFFSET" \
      "$dest"
    i=$((i + 1))
    total=$((total + 1))
  done
}

# Praia do Guincho
stamp_day 2026:07:14 10 38.73310 9.47290 \
  8623318 8623319 8623320 8623321 8623322 8623323 \
  8623346 8623347 8623348 8623349 8623350 8623351
# Praia Grande, Sintra
stamp_day 2026:07:15 9 38.81690 9.47810 \
  8623324 8623325 8623326 8623327 8623328 8623329 \
  8623352 8623353 8623354 8623355 8623356 8623357 8623358
# Praia de Carcavelos
stamp_day 2026:07:16 10 38.67920 9.33520 \
  8623331 8623332 8623333 8623334 8623335 8623336 8623337 8623345 \
  8623359 8623360 8623361 8623362 8623363 8623364

cat >"$OUT/STAGED-METADATA.txt" <<NOTE
Demo fixture, not a real trip record.
Photos: Kampus Production on Pexels (https://www.pexels.com/license/).
Capture dates, GPS, and the itinerary are invented and were written by
scripts/make-demo-trip.sh. Do not redistribute this folder.
NOTE

echo "demo trip: $total photos in $OUT"
