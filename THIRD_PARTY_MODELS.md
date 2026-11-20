# Third-Party Models Ledger

Every model shipped or downloaded by TripTape must have an entry here before
it lands (PLAN.md Section 13). No entry, no merge.

| Model | Version / weights | License | Size | Tier | Local/Cloud | Runtime | Status | Blocker | Fallback |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Face detector: YuNet 2026may | opencv_zoo `48b10fa505aeb51fbd127fc60a5c660040e823b7`, `models/face_detection_yunet/face_detection_yunet_2026may.onnx` (Git LFS), SHA-256 `ebafce4e3c118d6554634be5c27ab333b4c047a9a8c3faf1d7cf93101c22f0f0` | Code MIT (model dir LICENSE) / training code BSD-3. **Flag:** trained on WIDER Face, whose site lists CC BY-NC-ND | 229,738 B | Core | Local, optional download via `scripts/fetch-models` | ONNX Runtime CPU EP | PINNED, wired (face count, largest face size, gates FER+) | **Owner decision 2026-11-20: use it despite the WIDER Face training-data flag**; G4 baseline not measured | `faces = None`: no face or smile signals, scoring exactly as before |
| Expression: FER+ emotion-ferplus-8 | onnx/models `c5cae0f1942c8d7727372c7b1f6b485aa39e4421`, `validated/vision/body_analysis/emotion_ferplus/model/emotion-ferplus-8.onnx` (Git LFS), SHA-256 `a2a2ba6a335a3b29c21acb6272f962bd3d47f84952aaffa03b60986e04efa61c` | Model README MIT, repo Apache-2.0; FER+ labels (microsoft/FERPlus) licence unverified. **Flag:** images are Kaggle FER2013 with no stated licence (AMBIGUOUS) | 35,040,571 B | Core | Local, optional download via `scripts/fetch-models` | ONNX Runtime CPU EP | PINNED, wired (best size-weighted smile, faces >= 40 px only) | **Owner decision 2026-11-20: use it despite the FER2013 / label licence flags**; G4 baseline not measured | No smile signal |
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

Privacy: everything is computed on the cached thumbnail and stored only in
the local SQLite database: `assets.embedding` (384-byte int8 BLOB) and, per
photo, `face_count`, `face_size` (largest face / shorter side) and `smile`
(best size-weighted smile, 0..1). Face boxes and crops are never stored, and
none of it is written to plans, exports or logs. The embedding describes the
picture, not a person: no identity / face-recognition embeddings exist.

Pin note: the spike doc's truncated FER+ hash `a2a2ba6a...1a9` does not match
the suffix of the Git LFS object id at the pinned commit (`...efa61c`); size
(35,040,571 B) and prefix match, and the downloaded file matches the LFS oid,
so the doc's suffix looks like a typo. The pin above is the LFS oid.

## License traps (must stay out of the shipped binary)

- YOLO Ultralytics line: AGPL-3.0. Use Apache-2.0 alternatives instead.
- InsightFace buffalo weights: research-only, non-commercial. Do not ship.
- DINOv3 / SAM 3 weights: custom/gated licenses. Cloud opt-in only.

## FFmpeg (not a model, recorded here for convenience)

- Bundled static build, full GPL (`--enable-gpl --enable-version3`) with
  `libx264` + `libvidstab`. See `docs/ffmpeg-build.md` for the pinned
  source, version, SHA-256, and `-buildconf` output recorded in Sprint 0.
- No `--enable-nonfree` builds: they cannot be redistributed.
