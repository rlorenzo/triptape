#!/usr/bin/env bash
# Build a tiny synthetic, fully redistributable demo trip with lavfi inputs.
# No cameras, no people, no real locations: color bars + test patterns with
# synthetic timestamps spread over two "days". Safe to ship in the repo.
#
# Usage: ./scripts/make-fixture.sh [out-dir]   (default: fixtures/tiny-trip/)
set -euo pipefail

OUT="${1:-fixtures/tiny-trip}"
FF="${FFMPEG:-ffmpeg}"
mkdir -p "$OUT/day1" "$OUT/day2"

# 8 stills: testsrc2 frames at two sizes (mix of landscape/portrait).
for i in 1 2 3 4 5 6; do
  W=1920; H=1080; D=day1
  if [ "$i" -gt 3 ]; then D=day2; fi
  if [ $((i % 2)) -eq 0 ]; then W=1080; H=1920; fi
  "$FF" -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=${W}x${H}:rate=1:duration=1" \
    -frames:v 1 "$OUT/$D/still_$(printf '%02d' "$i").png"
done

# 4 short clips (3 s each): smooth color motion + sine audio, 2 landscape + 2 portrait.
for i in 1 2 3 4; do
  W=1280; H=720; D=day1
  if [ "$i" -gt 2 ]; then D=day2; W=720; H=1280; fi
  "$FF" -hide_banner -loglevel error -y \
    -f lavfi -i "smptehdbars=size=${W}x${H}:rate=30:duration=3" \
    -f lavfi -i "sine=frequency=$((440 + i * 110)):duration=3" \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
    "$OUT/$D/clip_$(printf '%02d' "$i").mp4"
done

echo "fixture: $(find "$OUT" -type f | wc -l) files in $OUT"
ls "$OUT/day1" "$OUT/day2"
