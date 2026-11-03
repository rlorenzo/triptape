# Third-Party Models Ledger

Every model shipped or downloaded by TripTape must have an entry here before
it lands (PLAN.md Section 13). No entry, no merge.

| Model | Version / weights | License | Size | Tier | Local/Cloud | Runtime | Status | Blocker | Fallback |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Face detector (YuNet/BlazeFace class) | NOT PINNED | Apache-2.0 alternative TBD (never YOLO-Ultralytics, AGPL-3.0) | - | Core (budget ≤250 MB combined) | Local | ONNX Runtime CPU (planned) | NOT PINNED | G4 model spike not run on the hardware baseline (8 GB RAM, 4-core CPU, no dGPU, both OSes) | Heuristic: `face_count=None` (unknown, never zero); face-or-center-weighted crop |
| Embedding (DINOv2-small class) | NOT PINNED | Apache-2.0 line TBD | - | Core (budget ≤250 MB combined) | Local | ONNX Runtime CPU (planned) | NOT PINNED | G4 model spike not run on the hardware baseline, both OSes | Content-hash + pHash dedupe; no embedding cosine-similarity grouping |
| Tagger (SigLIP/CLIP-small class) | NOT PINNED | Apache-2.0/commercial-safe TBD | - | Core (budget ≤250 MB combined) | Local | ONNX Runtime CPU (planned) | NOT PINNED | G4 model spike not run on the hardware baseline, both OSes | Quality heuristics only (sharpness, blur, exposure) |
| Transcription (Whisper small/turbo class) | NOT PINNED | TBD (commercial-safe) | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | No quote captions from speech |
| Subject masks (SAM 2.1 small class) | NOT PINNED | Apache-2.0 line TBD | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | Face-or-center-weighted crop instead of SAM masks |
| Draft captions (SmolVLM/Moondream class VLM) | NOT PINNED | Apache-2.0 line TBD | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | Template captions from plan context (Sprint 2, always editable) |

## License traps (must stay out of the shipped binary)

- YOLO Ultralytics line: AGPL-3.0. Use Apache-2.0 alternatives instead.
- InsightFace buffalo weights: research-only, non-commercial. Do not ship.
- DINOv3 / SAM 3 weights: custom/gated licenses. Cloud opt-in only.

## FFmpeg (not a model, recorded here for convenience)

- Bundled static build, full GPL (`--enable-gpl --enable-version3`) with
  `libx264` + `libvidstab`. See `docs/ffmpeg-build.md` for the pinned
  source, version, SHA-256, and `-buildconf` output recorded in Sprint 0.
- No `--enable-nonfree` builds: they cannot be redistributed.
