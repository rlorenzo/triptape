# Third-Party Models Ledger

Every model shipped or downloaded by TripTape must have an entry here before
it lands (PLAN.md Section 13). No entry, no merge.

| Model | Version / weights | License | Size | Tier | Local/Cloud | Runtime | Status | Blocker | Fallback |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Face detector (YuNet/BlazeFace class) | NOT PINNED | Apache-2.0 alternative TBD (never YOLO-Ultralytics, AGPL-3.0) | - | Core (budget ≤250 MB combined) | Local | ONNX Runtime CPU (planned) | NOT PINNED | G4 model spike not run on the hardware baseline (8 GB RAM, 4-core CPU, no dGPU, both OSes) | Heuristic: `face_count=None` (unknown, never zero); face-or-center-weighted crop |
| Embedding: DINOv2-small int8 | `onnx-community/dinov2-small@8b1f705a3a7f6f062f6bdd21986c1583d3ef105d`, `onnx/model_int8.onnx`, SHA-256 `dfce54a839b491f395c516350ebb4a78f947e9170a6beac0f2bc5638e0f09d61` | Apache-2.0 (facebookresearch/dinov2 LICENSE; HF card) | 24,446,700 B | Core (budget ≤250 MB combined) | Local, optional download via `scripts/fetch-models` | ONNX Runtime 1.31.1 CPU EP via `ort =2.0.0-rc.15` `load-dynamic` | PINNED, wired (near-duplicate "same scene" signal); dev Mac: ~87 ms/photo added, peak RSS +0.67 GB with 2 sessions | G4 baseline hardware not measured (dev-machine numbers only, see `docs/gates.md`) | Content-hash + pHash dedupe (exactly the pre-model behaviour) |
| Runtime: ONNX Runtime | 1.31.1 shared library, Microsoft GitHub release (`onnxruntime-<os>-<arch>-1.31.1`) | MIT | 45 MB (macOS arm64 dylib) | Core | Local, optional download via `scripts/fetch-models` | n/a | **BLOCKED** in `scripts/fetch-models`: the release URL returns 404 since at least 2026-11-20 and the spike kept only truncated digests (osx-arm64 `2e1344a1...6d6f`, win-x64 `5869e26b...4b2c`), so no full SHA-256 is pinned | Full digest from the release page | `TRIPTAPE_ORT_LIB` may point at a 1.31.x library the owner already trusts; without a runtime every model is off |
| Tagger (SigLIP/CLIP-small class) | NOT PINNED | Apache-2.0/commercial-safe TBD | - | Core (budget ≤250 MB combined) | Local | ONNX Runtime CPU (planned) | NOT PINNED | G4 model spike not run on the hardware baseline, both OSes | Quality heuristics only (sharpness, blur, exposure) |
| Transcription (Whisper small/turbo class) | NOT PINNED | TBD (commercial-safe) | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | No quote captions from speech |
| Subject masks (SAM 2.1 small class) | NOT PINNED | Apache-2.0 line TBD | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | Face-or-center-weighted crop instead of SAM masks |
| Draft captions (SmolVLM/Moondream class VLM) | NOT PINNED | Apache-2.0 line TBD | - | Optional (budget ≤1 GB combined) | Local | Planned on-demand download | NOT PINNED | G4 model spike not run; optional-tier download flow not built | Template captions from plan context (Sprint 2, always editable) |

## Local model files (not in git, not bundled, not fetched by the app)

`scripts/fetch-models [dir]` downloads each pinned file from a URL that names
an immutable revision, checks its SHA-256 and installs it into the app data
dir's `models/` (macOS `~/Library/Application Support/app.triptape.desktop/models`,
Windows `%APPDATA%\app.triptape.desktop\models`). A mismatch is a hard stop.
The app and the CLI look there, or in `--models DIR` (CLI) /
`TRIPTAPE_MODELS_DIR`; the runtime library is `TRIPTAPE_ORT_LIB` or
`<models>/onnxruntime/`. When anything is missing, TripTape prints one line
naming it and behaves exactly as without models. Rust side: `ort =2.0.0-rc.15`
with `default-features = false` (never `download-binaries`) and `load-dynamic`,
so nothing is fetched or linked at build time.

Privacy: embeddings are computed on the cached thumbnail, stored only in the
local SQLite database (`assets.embedding`, 384-byte int8 BLOB) and never
written to plans, exports or logs. They describe the picture, not a person:
no identity / face-recognition embeddings are computed.

## License traps (must stay out of the shipped binary)

- YOLO Ultralytics line: AGPL-3.0. Use Apache-2.0 alternatives instead.
- InsightFace buffalo weights: research-only, non-commercial. Do not ship.
- DINOv3 / SAM 3 weights: custom/gated licenses. Cloud opt-in only.

## FFmpeg (not a model, recorded here for convenience)

- Bundled static build, full GPL (`--enable-gpl --enable-version3`) with
  `libx264` + `libvidstab`. See `docs/ffmpeg-build.md` for the pinned
  source, version, SHA-256, and `-buildconf` output recorded in Sprint 0.
- No `--enable-nonfree` builds: they cannot be redistributed.
