#!/usr/bin/env bash
# FFmpeg sidecar spike (Sprint 0): Ken Burns + xfade + loudnorm render from
# generated lavfi inputs, then ffprobe-verify. No repo media required.
# Output: /tmp/spike-render.mp4
set -euo pipefail
FF="${FFMPEG:-ffmpeg}"
FP="${FFPROBE:-ffprobe}"
OUT="/tmp/spike-render.mp4"
rm -f "$OUT"

"$FF" -hide_banner -y \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=3" \
  -f lavfi -i "smptehdbars=size=1920x1080:rate=30:duration=3" \
  -f lavfi -i "sine=frequency=440:duration=3" \
  -f lavfi -i "sine=frequency=660:duration=3" \
  -filter_complex "\
[0:v]scale=2400:-2,zoompan=z='1+0.12*on/90':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=90:s=1920x1080:fps=30[v0];\
[1:v]scale=2400:-2,zoompan=z='1.12-0.12*on/90':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=90:s=1920x1080:fps=30[v1];\
[v0][v1]xfade=transition=fade:duration=0.5:offset=2.5[v];\
[2:a][3:a]acrossfade=d=0.5[a];[a]loudnorm=I=-16:TP=-1.5:LRA=11[aout]" \
  -map "[v]" -map "[aout]" \
  -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p \
  -c:a aac -movflags +faststart -shortest "$OUT"

echo "== ffprobe verify =="
"$FP" -hide_banner -v error -show_entries stream=codec_name,width,height \
  -show_entries format=duration,size -of default=noprint_wrappers=1 "$OUT"
probe() { # probe <stream-selector> <entries>
  "$FP" -v error -select_streams "$1" -show_entries "$2" -of csv=p=0 "$OUT"
}
DUR="$("$FP" -v error -show_entries format=duration -of csv=p=0 "$OUT")"
IFS=, read -r W H <<<"$(probe v:0 stream=width,height)"
VC="$(probe v:0 stream=codec_name)"
AC="$(probe a:0 stream=codec_name)"
[ "$W" = 1920 ] && [ "$H" = 1080 ] || { echo "FAIL: expected 1920x1080, got ${W}x${H}"; exit 1; }
[ "$VC" = h264 ] || { echo "FAIL: expected h264 video, got $VC"; exit 1; }
[ "$AC" = aac ] || { echo "FAIL: expected aac audio, got $AC"; exit 1; }
awk -v d="$DUR" 'BEGIN { exit !(d >= 5.0 && d <= 6.0) }' \
  || { echo "FAIL: expected ~5.5s duration, got $DUR"; exit 1; }
echo "PASS: $OUT (${W}x${H} $VC+$AC ${DUR}s)"
