# FFmpeg Build (Gate G1)

PLAN.md Section 7. The sidecar is a full GPL static build
(`--enable-gpl --enable-version3`) with `libx264` + `libvidstab`.
No `--enable-nonfree` build may ever ship (not redistributable).

## System build on this machine (reference only, NOT the sidecar)

- `ffmpeg 9.0.2` (Homebrew, Apple clang, arm64), `ffprobe 9.0.2`,
  `exiftool 13.55`, `typst 0.15.1`.
- Full `-buildconf` is reproducible via `ffmpeg -hide_banner -buildconf`.
  Key flags: `--enable-gpl --enable-version3 --enable-libx264
  --enable-videotoolbox --enable-audiotoolbox`. No `--enable-nonfree`.
- `scripts/check-ffmpeg.sh` result today: PASS on all required items
  (`libx264`, native `aac`, `xfade`, `zoompan`, `loudnorm`).
- Reported gaps (warn-only, see Gates G1): `zscale`, `drawtext`,
  `vidstabdetect`/`vidstabtransform`, `libass` all missing from this
  Homebrew build.

## Pinned sidecar (what ships)

| Target | Source | Version | SHA-256 |
| --- | --- | --- | --- |
| macOS arm64 | evermeet.cx static build | 8.0.1 | `<FILL in Sprint 1>` |
| macOS x86_64 | evermeet.cx static build | 8.0.1 | `<FILL in Sprint 1>` |
| Windows x64 | BtbN `win64-gpl-shared` full build (fallback: gyan.dev full) | 8.0.1 | `<FILL in Sprint 1>` |

## Sidecar pin procedure

1. Download the build for each target from the source above.
2. Verify the publisher's published checksum out of band.
3. Paste the SHA-256 into `scripts/fetch-ffmpeg` (replacing the
   `<FILL…>` placeholder) and into the table above.
4. Run `scripts/fetch-ffmpeg`, then `scripts/check-ffmpeg.sh <sidecar>`.
   Required encoders/filters must PASS; missing `zscale`/`drawtext`/
   `vidstab`/`libass` are recorded as G1 gaps (see `docs/gates.md`).
5. Record the sidecar's `-buildconf` output in this file.
6. CI runs `check-ffmpeg.sh` per bundled binary per target and fails the
   build on `--enable-nonfree` or a missing required encoder/filter.

## Consequences of the current gaps

- No `zscale`: HDR (HLG/PQ, iPhone default) tone-map path from PLAN
  Section 7 (`zscale` + `tonemap`) cannot run as specified. Short-term:
  `tonemap` alone (present) or format-convert fallback; revisit when the
  pinned sidecar restores `zscale`.
- No `drawtext`/`libass`: burned-in captions need `libass` (ASS subtitles)
  in the pinned sidecar; the system build cannot do them.
- No `vidstab`: two-pass stabilization is unavailable until the sidecar
  lands; stabilization stays off by default per plan.
