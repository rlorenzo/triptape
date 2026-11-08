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
  (`libx264`, native `aac`, `xfade`, `perspective`, `loudnorm`).
- Reported gaps (warn-only, see Gates G1): `zscale`, `drawtext`,
  `vidstabdetect`/`vidstabtransform`, `subtitles`/`ass` (libass) all
  missing from this plain Homebrew build. The `ffmpeg-full` formula
  (`/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg`) has `subtitles`, `ass` and
  `drawtext`.

## Download attempt (2026-11-03, Sprint 6, macOS arm64 dev machine)

- URL followed (302 → `https://e.deolaha.ca:4242/pub/ffmpeg/ffmpeg-8.0.1.7z`):
  downloaded 17,770,148 bytes, SHA-256
  `845140e046f7abbfcf480d70eb1657ca09eb8fa775834518a1f43a5b867c96f9`.
- `file` reports **Mach-O 64-bit executable x86_64** — an Intel-only build.
  It does not execute on this arm64 machine (`Bad CPU type in executable`;
  Rosetta not installed). Capability probing (`-buildconf`, `-encoders`,
  `-filters`) was therefore **not possible**, and the hash above is the
  hash of the downloaded archive, **not** a verified sidecar pin.
- NOT pinned: `scripts/fetch-ffmpeg` placeholders stay in place. Either
  install Rosetta and re-probe, or source a native arm64 static build
  (evermeet publishes Intel-only builds), then pin per the procedure below.

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
   `vidstab`/`subtitles` are recorded as G1 gaps (see `docs/gates.md`).
5. Record the sidecar's `-buildconf` output in this file.
6. CI runs `check-ffmpeg.sh` per bundled binary per target and fails the
   build on `--enable-nonfree` or a missing required encoder/filter.

## Consequences of the current gaps

- No `zscale`: HDR (HLG/PQ, iPhone default) tone-map path from PLAN
  Section 7 (`zscale` + `tonemap`) cannot run as specified. Short-term:
  `tonemap` alone (present) or format-convert fallback; revisit when the
  pinned sidecar restores `zscale`.
- No libass (`subtitles`/`ass` filters; ffmpeg has no filter named
  `libass`): the hero film writes captions as a sidecar SRT only. With a
  libass build the SRT is also burned in. The app never uses `drawtext`;
  title and lower-third text are Typst PNG overlays.
- No `vidstab`: two-pass stabilization is unavailable until the sidecar
  lands; stabilization stays off by default per plan.
