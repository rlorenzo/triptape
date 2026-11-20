# Go/No-Go Gates (PLAN.md Section 17, Sprint 0)

All five gates are still open. They now gate the public release (`PLAN.md`
Section 23) rather than the renderer sprints, which shipped for the hackathon.

No keepsake renderer sprint (3–5) starts until G1 passes. Sprint 5 does not
start until G2 has a recorded outcome.

| Gate | Owner | Pass criteria | Fallback if it fails |
| --- | --- | --- | --- |
| G1 FFmpeg build | TBD (Rex) | CI build check (`scripts/check-ffmpeg.sh`) passes for macOS arm64/x86_64 and Windows x64; `libx264` 1080p30 encode works on a no-GPU baseline; output plays in QuickTime, WMP, and a phone | Switch to another pinned static build source; hardware encoders default only if `libx264` too slow on baseline |
| G2 Vendor workflow | TBD (Rex) | Per vendor × preset: designer accepts page JPEGs at product size with ≤2% visible crop, order preservable, steps documented | Drop failing vendor/preset from UI; promise narrows to passing vendors + generic press PDF |
| G3 Offline maps | TBD (Rex) | Route card renders from bundled tiles with networking off; basemap ≤60 MB | Pins + route on plain styled background (no basemap, no attribution needed) |
| G4 Model budgets | TBD (Rex) | Core tier meets Section 13 disk/RAM/throughput ceilings on both OSes | Swap to smaller model class or optional tier; heuristic-only scoring last resort |
| G5 Signed update | TBD (Rex) | Signed build installs clean (no Gatekeeper/SmartScreen block beyond first-run reputation); one updater hop via test channel | Manual-download updates for the hackathon; updater work stays in Sprint 6 |

## Evidence (Sprint 0, 2026-11-03, macOS arm64 dev machine)

- G1 (local, system ffmpeg — NOT the sidecar): `scripts/check-ffmpeg.sh`
  PASS on required (`libx264`, native `aac`, `xfade`, `perspective`,
  `loudnorm`); gaps reported: `zscale`, `drawtext`, `vidstab`,
  libass (`subtitles`/`ass`) missing. `scripts/spike-render.sh` PASS: Ken Burns
  (`zoompan`) + `xfade` + `acrossfade` + `loudnorm` render from lavfi
  inputs → `/tmp/spike-render.mp4`, ffprobe-verified 1920x1080 h264+aac
  5.5 s. No-GPU baseline + Windows + phone-playback checks still open.
  → G1 **not passed** (sidecar unpinned, cross-OS unverified).
- G1 update (R2, 2026-11-20, `feat/self-contained-app`): sidecars pinned
  (FFmpeg 9.0.2 from evermeet.cx on macOS, GPG-verified; gyan.dev
  essentials 9.0.2 on Windows, checksum-verified; Typst 0.15.1). Details and
  hashes: `docs/ffmpeg-build.md`.
  - macOS, arm64 dev Mac, the Intel sidecar under Rosetta 2:
    `scripts/check-ffmpeg.sh` PASS with no gaps (`zscale`, `drawtext`,
    `vidstab`, `subtitles`/`ass` all present). `scripts/make-fixture.sh` +
    `scripts/spike-render.sh` with the sidecar and no FFmpeg on `PATH`:
    PASS, 1920x1080 h264+aac 5.5 s. `cargo test --workspace` with `PATH`
    stripped of Homebrew and `TRIPTAPE_REQUIRE_SIDECAR=1`: all pass, the
    sidecar capability test and the libass caption burn-in test ran against
    the sidecar. A CLI export of a fixture trip with a captioned photo
    burned the caption into the 1080p film (frame inspected). HEIC with
    `sips` disabled (the Windows route): nine iPhone photos, six of them
    Dolby Vision `tmap` gain-map files, decoded upright at full size,
    mean pixel difference from ImageIO under 1/255. libx264 1080p30
    `-preset medium` encode: 4.1 s for 10 s of video under Rosetta, 2.3 s
    with native Homebrew FFmpeg on the same Mac.
  - macOS x86_64: same binaries, not run on an Intel Mac. Pending.
  - Windows x64: **pending CI** (fetch, `check-ffmpeg.sh`, tests and
    fixture render against the sidecar are wired into `.github/workflows/ci.yml`).
  - Still open: no-GPU baseline machine, playback in QuickTime, WMP and a
    phone, and the Apple silicon decision (native build vs Rosetta).
  → G1 **not passed** yet (Windows and playback evidence outstanding).
- G2: not run (needs vendor designer pass + physical proof). Open.
- G3: not run (basemap unbuilt). Open.
- G4: not run (models unpinned; ledger still `(none pinned yet)`). Open.
- G5: not run (no signing keys / test channel yet). Open.
- Typst present: `typst 0.15.1`. Book spike PASS: `fixtures/book-spike.typ`
  → `/tmp/book-spike/proof.pdf` (4 pp) + 4 JPEGs, ffprobe-asserted
  3000x2400 each, via `scripts/spike-book.sh`.
- `cargo build --workspace` PASS; `cargo test --workspace` 24/24 PASS.

## G4 evidence: local models wired (2026-11-20, dev machine only)

Measured on the Apple M-series / 24 GB dev Mac, **not** the G4 baseline
(8 GB, 2019 4-core, no dGPU, Windows), so G4 stays open. Trip: `Japan to
Singapore 2026` (4,913 files, 2,931 assets, 2,883 stills), CLI
`triptape scan`, CPU EP, ONNX Runtime 1.31.1 loaded from a local copy via
`TRIPTAPE_ORT_LIB` (the release download pin is blocked, see
`THIRD_PARTY_MODELS.md`).

- Stage 1, DINOv2-small int8 embeddings on the 320 px thumbnails, 4 scoring
  workers sharing 2 sessions: 186 ms per inference per session (about
  87 ms/photo of added wall time). Stills pass 53 s -> 303 s; whole warm
  scan 142 s -> 459 s; peak RSS 0.68 GB -> 1.35 GB (4 sessions reached
  1.85 GB, over the 1.5 GB ceiling, hence `MAX_SESSIONS = 2`). Embeddings
  stored as 384-byte int8 BLOBs (about 1.1 MB for this trip).
- Cosine distribution: random pairs median 0.12 (p95 0.36); same-minute pairs
  median 0.79; pHash near-duplicates median 0.97. Contact sheets showed
  0.6-0.85 is "same place", so thresholds are 0.90 for the 2-minute
  same-moment check and 0.95 for near-duplicate collapse.
- Near-duplicate collapse: pHash only 1,145 groups / 1,538 suppressed; with
  embeddings 1,149 / 1,542. With the spike's 0.6 it was 951 / 1,722.
- Stage 2, YuNet + FER+ on the same thumbnails (all three models, same
  machine, warm cache): DINOv2 127 ms/run, YuNet 7.4 ms/photo, FER+ 0.7 ms
  per readable face (942 faces). Stills pass 53 s -> 214 s (about 56 ms/photo
  added for all three models); whole warm scan 142 s -> 357 s; peak RSS
  1.41 GB. 1,872 of 2,883 stills have a plausible face (posters and statues
  included, hence no people count from it), 742 have a face of 40 px or more
  to read, 421 earn "smiling" and 369 untagged ones "face in frame". Top 50
  stills by score: 11 moved in (all "smiling"; four group selfies checked by
  eye, all smiling), 11 moved out.
