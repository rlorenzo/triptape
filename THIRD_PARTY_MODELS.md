# Third-Party Models Ledger

Every model shipped or downloaded by TripTape must have an entry here before
it lands (PLAN.md Section 13). No entry, no merge.

| Model | Version / weights | License | Size | Tier | Local/Cloud | Runtime | Fallback |
| --- | --- | --- | --- | --- | --- | --- | --- |
| (none pinned yet) | Sprint 0 spike | - | - | Core | Local | ONNX Runtime CPU | Heuristic scoring |

## License traps (must stay out of the shipped binary)

- YOLO Ultralytics line: AGPL-3.0. Use Apache-2.0 alternatives instead.
- InsightFace buffalo weights: research-only, non-commercial. Do not ship.
- DINOv3 / SAM 3 weights: custom/gated licenses. Cloud opt-in only.

## FFmpeg (not a model, recorded here for convenience)

- Bundled static build, full GPL (`--enable-gpl --enable-version3`) with
  `libx264` + `libvidstab`. See `docs/ffmpeg-build.md` for the pinned
  source, version, SHA-256, and `-buildconf` output recorded in Sprint 0.
- No `--enable-nonfree` builds: they cannot be redistributed.
