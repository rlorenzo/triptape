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
- G2: not run (needs vendor designer pass + physical proof). Open.
- G3: not run (basemap unbuilt). Open.
- G4: not run (models unpinned; ledger still `(none pinned yet)`). Open.
- G5: not run (no signing keys / test channel yet). Open.
- Typst present: `typst 0.15.1`. Book spike PASS: `fixtures/book-spike.typ`
  → `/tmp/book-spike/proof.pdf` (4 pp) + 4 JPEGs, ffprobe-asserted
  3000x2400 each, via `scripts/spike-book.sh`.
- `cargo build --workspace` PASS; `cargo test --workspace` 24/24 PASS.
