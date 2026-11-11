# Spike: face / smile / embedding models (Gate G4 evidence)

Branch `spike/face-embedding-models`. Research plus a measured prototype
(`spikes/face-embedding/`, feature `spike`). Nothing here is wired into
scoring and no weights are committed (they live in git-ignored `output/`).
Dates: 2026-11-08. Machine: Apple M-series, 24 GB, macOS 27.2, **not** the
G4 baseline (8 GB / 2019 4-core / no dGPU), so latency is lower-bound
evidence only.

## Recommendation

Pin these (CPU EP, `ort` + ONNX Runtime 1.31.1 loaded dynamically):

| Slot | Pin | Size | Licence verdict |
| --- | --- | --- | --- |
| Face detection | YuNet `face_detection_yunet_2026may.onnx` (opencv_zoo `48b10fa`) | 0.23 MB | Code MIT/BSD-3 clean. Weights trained on WIDER Face, whose site says CC BY-NC-ND: **flag for a decision**, no clean alternative found |
| Expression / smile | FER+ `emotion-ferplus-8.onnx` (onnx/models `c5cae0f`) | 35 MB | Model MIT/Apache-2.0, labels MIT, **images are Kaggle FER2013 with no stated licence: AMBIGUOUS**. Fallback: CLIP zero-shot (MIT, weaker) |
| Near-duplicate embedding | DINOv2-small int8 (`onnx-community/dinov2-small@8b1f705a3a`, `model_int8.onnx`) | 24.4 MB (fp32 88.5 MB) | Apache-2.0, clean |
| Runtime | ONNX Runtime 1.31.1 shared lib | 45 MB (macOS arm64 dylib) | MIT |

Total for the three models: **about 60 MB**; with the runtime about 105 MB, so
the 250 MB core budget has room for the tagger slot (CLIP ViT-B/32 vision int8,
89 MB, vision tower only) with about 45 MB spare. Eyes-open has **no
licence-clean small model**: CLIP prompts did not work (see below). Leave it
out of the first cut.

G4 verdict from this spike: disk and RAM fit on paper; **latency on the
baseline is unmeasured**, so G4 stays open for baseline hardware. Measured
cost here is about 0.15 s/photo for detector + FER+ + DINOv2, about 3x that
estimated on the baseline, so about 7 to 8 min per 1,000 photos before the
tagger. That is tight against the 10 min ceiling, so plan to run the embedding
on a small thumbnail, only for photos that survive the cheap filters.

## Candidates and verified licences

Everything below was read from the actual LICENSE / model card on 2026-11-08.
"Not measured" rows were evaluated on paper only.

### (a) Face detection

| Candidate | Source / version | Size | Licence (code / weights) | ONNX | Input | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| **YuNet 2026may** (dynamic shape) | https://github.com/opencv/opencv_zoo `models/face_detection_yunet/` @ `48b10fa505aeb51fbd127fc60a5c660040e823b7` (LFS). sha256 `ebafce4e...f0f0` | 229,738 B | Dir `LICENSE` = MIT (Shiqi Yu); training repo libfacedetection.train = BSD-3. No separate weights licence. Trained on WIDER Face: dataset page lists CC BY-NC-ND | yes | any H,W (benchmarked 640 long edge, padded to /32), BGR 0-255 | **Pin**, flag data provenance |
| YuNet 2023mar | same commit, sha256 `8f2383e4...2fa4` | 232,589 B | as above | yes | fixed 640x640 | superseded by 2026may (same weights) |
| SCRFD | InsightFace | n/a | `model_zoo/README.md`: "ALL models are available for non-commercial research purposes only" | yes | | **Excluded** (InsightFace origin, non-commercial) |
| MediaPipe BlazeFace / Face Landmarker | Google | not fetched | Apache-2.0 code; TFLite only, no ONNX | no | | Not evaluated; conversion needed |

### (b) Smile / expression / quality

| Candidate | Source / version | Size | Licence (code / weights / data) | ONNX | Input | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| **FER+ emotion-ferplus-8** | https://github.com/onnx/models `validated/vision/body_analysis/emotion_ferplus` @ `c5cae0f1942c8d7727372c7b1f6b485aa39e4421` (LFS). sha256 `a2a2ba6a...1a9` | 35,040,571 B | README "License MIT"; repo LICENSE Apache-2.0; labels from microsoft/FERPlus (no LICENSE file found at `master/LICENSE` (404); its README states no licence, so the label licence is **unverified**). Images: Kaggle FER2013, no licence stated | yes | 1x1x64x64 grayscale 0-255 | **Best signal; licence AMBIGUOUS (FER2013 images)** |
| CLIP ViT-B/32 zero-shot on face crop | OpenAI weights via `Xenova/clip-vit-base-patch32@6b7c523703` | vision int8 88.6 MB, text int8 64.1 MB | OpenAI/CLIP repo MIT; HF card `license: mit`. Weights MIT, but the OpenAI model card says any deployed use is out of scope and that the training data is not intended for commercial or deployed models: **flag for the tagger decision** | yes | 224x224 | Licence-clean fallback; weak (below) |
| OpenCV zoo FER (MobileFaceNet) | opencv_zoo `facial_expression_recognition` @ `48b10fa` | 4.8 MB | Repo root Apache-2.0, no model-dir LICENSE; README states RAF-DB, which says non-commercial research only | yes | 112x112 aligned | **Excluded** (RAF-DB NC) |
| HSEmotion / AffectNet models | n/a | not fetched | AffectNet is non-commercial | | | **Excluded** by brief |
| `onnx-community/face-emotion-detection-ONNX` | HF | not fetched | Apache-2.0 card, trained on FER2013 | yes | | Same data ambiguity as FER+, no benefit |
| Eyes-open / blink | none found with clean licence and ONNX | | MediaPipe blendshapes are TFLite | | | Open gap |

### (c) Embeddings for near-duplicates

| Candidate | Source / version | Size | Licence (code / weights) | ONNX | Input | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| **DINOv2-small** | `onnx-community/dinov2-small@8b1f705a3a` (export of `facebook/dinov2-small`) | int8 24,446,700 B (sha256 `dfce54a8...9d61`), fp32 88,532,934 B (`f22797ea...c20a`) | facebookresearch/dinov2 LICENSE Apache-2.0; HF card `apache-2.0` (onnx-community card has no licence field, inherits base) | yes | 224x224 ImageNet norm | **Pin** (int8; cosine vs fp32 mean 0.987, min 0.964 over 40 photos) |
| CLIP ViT-B/32 vision | as above | int8 88.6 MB (`0ab0c1b3...bd93`), fp32 352 MB | MIT, plus the same model-card caveat | yes | 224x224 CLIP norm | OK, bigger, worse separation (below). Needed anyway if CLIP is the tagger |
| OpenCLIP ViT-B/32 (LAION) | not fetched | same architecture and size | MIT code; LAION weights MIT per card, not re-verified | convertible | | Not needed, same trade-off as OpenAI weights |
| MobileCLIP-S0/S2 | `apple/MobileCLIP-S0` (+ `Xenova/mobileclip_s0` ONNX, vision int8 11.8 MB) | | Apple ML Research Model licence: research use | yes | | **Excluded** (not commercial) |
| MobileNetV2 (stand-in for MobileNetV3/EffNet-lite) | onnx/models `mobilenet/model/mobilenetv2-12.onnx` @ `c5cae0f` | 13,964,571 B | Apache-2.0 (repo LICENSE); ImageNet-1k trained | yes | 224x224 | Latency baseline only: the zoo file exposes 1000 logits, not features, so it is not a usable embedder here. timm `mobilenetv3_large_100` / `tf_efficientnet_lite0` (Apache-2.0, 22 / 19 MB) ship safetensors only; ONNX export needs torch, not done |
| DINOv3, SAM3 | | | gated / custom | | | **Excluded** (Design Decisions) |

## `ort` and linking

- Latest `ort` on crates.io is **2.0.0-rc.15** (2026-10-19); no stable 2.0 yet.
  Pin exactly (`=2.0.0-rc.15`), edition 2024, MSRV 1.92, MIT OR Apache-2.0. It
  targets ONNX Runtime 1.31; the 1.31.1 release works.
- **Default features include `download-binaries`**: the build script silently
  fetches a prebuilt ONNX Runtime from pyke's CDN. Do not use it.
- Used here instead: `default-features = false, features = ["std","ndarray","load-dynamic","api-31"]`.
  Nothing is downloaded or linked at build time; the app calls
  `ort::init_from(<path to libonnxruntime>)` at startup (this prototype reads
  `ORT_DYLIB_PATH`). The Microsoft release tarball is verified by sha256
  (`onnxruntime-osx-arm64-1.31.1.tgz` = `2e1344a1...6d6f`, win-x64 zip
  `5869e26b...4b2c`, linux-x64 `c849e36a...bdc9`, from the GitHub release digests).
  Tauri would bundle the shared library as a resource and pass its path.
- The `Environment` is now explicit: `Session::builder(&env)`.
- macOS: CoreML EP (`coreml` feature) compiles and registers. Windows:
  `directml` feature needs a DirectML-enabled ONNX Runtime build (separate
  Microsoft package) and was **not tested**; CPU EP is the guaranteed path.
  Linux: CPU only (11 MB tgz). Not verified: a Windows machine may have an older
  inbox `onnxruntime.dll`, so always load by absolute path.

## Measurements (CPU EP, macOS arm64, 39 timed photos at 1280 px long edge)

One process per model; `load` is session creation (warm file cache), `first`
the first run, others are mean/p95 per image. RSS delta is peak RSS above the
image-cache baseline (about 233 MB of decoded test photos).

| Model | Threads | Load ms | First ms | Mean ms | p95 ms | Peak RSS delta MB |
| --- | --- | --- | --- | --- | --- | --- |
| YuNet 2026may (640 long edge) | 1 / 2 | 161 / 165 | 15 | 8.8 / 9.0 | 9.7 / 10.0 | 86 |
| FER+ (64x64 crop) | 1 / 2 | 277 / 275 | 1.1 | 0.6 / 0.6 | 0.6 / 0.7 | 283 |
| MobileNetV2 | 1 / 2 | 468 / 398 | 6.8 | 6.2 / 6.2 | 6.5 / 6.4 | 125 |
| DINOv2-small int8 | 1 / 2 | 605 / 604 | 140 to 150 | 90.5 / 81.5 | 90.9 / 92.1 | 385 |
| DINOv2-small fp32 | 1 / 2 | 386 / 354 | 115 to 133 | 82 / 89 | 100 / 115 | 445 |
| CLIP B/32 vision int8 | 1 / 2 | 1296 / 1431 | 53 to 71 | 61 / 51 | 67 / 70 | 208 |

Caveats: other agents were still using CPU during the first pass, so a
second pass was noisy (CLIP int8 35 ms vs 51 ms, DINO fp32 70 vs 89 ms). The
quietest pass gave CLIP int8 35 to 45 ms, DINO int8 79 to 91 ms. Treat as
"about" numbers. Cold first load of an int8 model took 5 to 6 s (disk cache).
2 threads barely beat 1 thread (small ops, int8 on Apple silicon without
VNNI), so run single-threaded and parallelise across photos. Quantised int8 is
not faster than fp32 on this ARM CPU; expect the opposite on x86 with VNNI/AVX2
(not measured).

**CoreML EP: no benefit.** YuNet 17 vs 9 ms, DINO fp32 170 vs 89 ms, CLIP int8
64 vs 51 ms (worse, with higher peak RSS and a first-run compile cost). Ship the
CPU EP; revisit CoreML only for a Mac fast path that is calibrated to the same
scores.

End-to-end `analyze` over 40 photos (detector, FER+ and CLIP crop per face,
DINO and CLIP per photo, 96 faces, 2 threads, includes JPEG decode and text
model start-up): **19 s, 0.48 s/photo, peak RSS 814 MB** with five sessions
resident. That includes CLIP-per-face; the pinned path (no CLIP) is about 0.15 s/photo.

Budget fit on the 8 GB / 4-core baseline (estimate, not measured): RAM is fine
(under 1 GB with all sessions, ceiling 1.5 GB). Time is the risk (see
recommendation). Disk fits.

## Quality sanity check (40 photos from `Japan to Singapore 2026`, read-only)

Source HEICs converted with `sips` into git-ignored `output/photos` (every 37th
HEIC plus 5 burst neighbours). Outputs: `output/analysis/analysis.json` and
annotated images. I viewed about 14 of them to check against truth.

**Face counts.** 96 faces in 32 of 40 photos; the 8 photos with zero faces
(food, sea view, lantern, souvenir cone, a 3:1 panorama) all have no visible
people, so no false negatives seen. Spot checks:

| Photo | Visual truth | YuNet |
| --- | --- | --- |
| IMG_0131 selfie | 2 | 2 |
| IMG_6312 group at Goofy | 4 | 4 |
| IMG_5468 Buddha group | 3 near, 1 statue, about 9 crowd | 13 (statue included) |
| IMG_1973 toy store | 2 real children | 5 (Elsa and Anna posters, one doll) |

So detection recall is good including small crowd faces (down to about 15 px).
Weakness: **stylised and statue faces count** (score 0.75 to 0.94). Do not use
raw count as "people"; weight by size and require FER+ plausibility.

**Smile (visual truth vs score).**

| Photo | Truth | FER+ happy | CLIP smile |
| --- | --- | --- | --- |
| IMG_0131 | both laughing | 1.00 / 0.98 | 0.83 / 0.81 |
| IMG_6312 | 3 smiling, toddler neutral | 1.00, 1.00, 1.00, 0.23 | 0.84, 0.87, 0.83, 0.59 |
| IMG_1973 | one shouting, toddler neutral, posters | all at or below 0.01 | 0.36 to 0.65; neutral toddler scored 0.65 (false smile) |
| IMG_5468 | grandma/girl smile, toddler neutral | 0.44, 0.68 near; toddler 0.00 | compressed 0.47 to 0.79 |

FER+ separates smiling from not cleanly (near-bimodal 0 / 1). CLIP zero-shot
has a compressed range (0.24 to 0.90), confuses neutral with smile, and
correlates 0.68 with FER+ over the 53 faces at least 60 px wide. The "eyes
open vs closed" prompt pair returned 0.31 to 0.67 for every face, i.e. no
signal. Faces under about 40 px are unreliable for expression; gate on size.

**Near-duplicates pHash misses** (dHash 64-bit Hamming distance, 0 = identical,
anything above about 10 treated as different). 780 pairs:

| Pair | What it is | DINOv2 cos | CLIP cos | dHash dist |
| --- | --- | --- | --- | --- |
| IMG_6303 / IMG_6312 | same Goofy group shot, different framing and zoom | **0.87** | 0.88 | 27 |
| IMG_1372 / IMG_1373 | same lantern, person vs close-up | **0.77** | 0.72 | 37 |
| IMG_5463 / IMG_5468 | same temple stop, different framing | 0.66 | 0.79 | 29 |

pHash distinguishes none of these. DINOv2 gives much better separation: median
over all pairs 0.12, so 0.6 or above looked like "same scene"; CLIP's median is
0.53 and same-place generic scenes score high (two empty-sea shots, 0.86 CLIP vs
0.41 DINO), so CLIP needs a high threshold or a second signal. Start with
DINOv2 cos at least 0.6 as the `same_moment` input and tune on a labelled set.

## Not done / limits

- G4 baseline hardware (8 GB, 2019 4-core, Windows and Linux) was not available.
- Windows DirectML and Linux were not run.
- Smile truth is visual on about 14 photos, not a labelled benchmark.
- MobileNetV3 / EfficientNet-lite ONNX features were not produced (no torch
  or ONNX tooling installed); MobileNetV2 logits only for latency.
- The prototype is a spike: HEIC must be pre-converted (no HEIC decode), single
  image path, hard-coded thresholds.

## Reproduce

```sh
export ORT_DYLIB_PATH=.../libonnxruntime.1.31.1.dylib MODEL_DIR=output/models SPIKE_THREADS=2
cargo run --release --manifest-path spikes/face-embedding/Cargo.toml --features spike -- bench yunet photos/*.jpg
cargo run --release --manifest-path spikes/face-embedding/Cargo.toml --features spike -- analyze output/analysis photos/*.jpg
```
Add `--features spike,coreml` and `SPIKE_EP=coreml` for the CoreML EP.
Model URLs and checksums are in the tables above; `output/models/SHA256SUMS.txt`
has the downloaded files' sha256.
