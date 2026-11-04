#!/usr/bin/env bash
# CI build check for the FFmpeg sidecar (PLAN.md Section 7 / gate G1).
# Fails on --enable-nonfree or a missing required encoder/filter.
# Missing nice-to-have filters (zscale/drawtext/vidstab/libass) are REPORTED
# as gate gaps but do not fail the check.
#
# Usage: ./scripts/check-ffmpeg.sh [ffmpeg-bin]   (default: ffmpeg on PATH)
set -u
FF="${1:-ffmpeg}"
fail=0
need() { # need <desc> <actual>
  if [ -z "$2" ]; then echo "FAIL: missing required $1"; fail=1;
  else echo "ok: $1 -> $2"; fi
}

echo "== ffmpeg: $FF =="
"$FF" -hide_banner -version | head -2
CONF="$("$FF" -hide_banner -buildconf 2>&1)"
echo "$CONF" | grep -q -- "--enable-nonfree" && { echo "FAIL: --enable-nonfree present (not redistributable)"; fail=1; }
echo "$CONF" | grep -q -- "--enable-gpl" || { echo "FAIL: --enable-gpl missing"; fail=1; }
ENC="$("$FF" -hide_banner -encoders 2>&1)"
FLT="$("$FF" -hide_banner -filters 2>&1)"
need "encoder libx264"        "$(echo "$ENC" | grep -o 'libx264' | head -1)"
need "encoder aac (native)"   "$(echo "$ENC" | grep -o ' AAC ' | head -1)"
need "filter xfade"           "$(echo "$FLT" | grep -o ' xfade ' | head -1)"
need "filter perspective"     "$(echo "$FLT" | grep -o ' perspective ' | head -1)"
need "filter loudnorm"        "$(echo "$FLT" | grep -o ' loudnorm ' | head -1)"
echo "== gate gaps (warn only) =="
for f in zscale drawtext vidstabdetect vidstabtransform libass; do
  if echo "$FLT $ENC" | grep -qw "$f"; then echo "ok: $f present";
  else echo "GAP: $f missing (HDR tone-map path / burned-in captions / stabilization affected)"; fi
done
if [ "$fail" -eq 0 ]; then echo "PASS: required FFmpeg capabilities present"
else echo "FAIL: required capabilities missing"; fi
exit "$fail"
