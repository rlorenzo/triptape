# TripTape — Drop in the trip. Get the story back.

Free, open-source desktop app (Tauri v2 + Rust) that turns a messy folder of
family vacation photos/videos into three keepsakes from one curation pass:
a Trip Video (4K hero film, 1080p optional via `export --resolution 1080p`), a Reels Pack (3–5 vertical clips), and a
Photo Book (print-ready PDF + per-page JPEGs). See [PLAN.md](PLAN.md) —
the source of truth until code exists.

## Run (Sprint 6)

Prerequisites: Rust toolchain, Node 20 + npm, `exiftool` 13.55, and the
pinned FFmpeg/ffprobe 9.0.2 and Typst 0.15.1 sidecars from
`./scripts/fetch-sidecars` (on an Apple silicon Mac the pinned FFmpeg needs
Rosetta 2). Without the sidecars, builds still work and the app falls back
to `ffmpeg`/`ffprobe`/`typst` on `PATH`, with a note on the Export screen.
No signing keys or vendor accounts needed.

```sh
cargo build --workspace     # all Rust crates incl. Tauri glue
cargo test --workspace      # core + media + glue unit tests
npm install && npm run build  # frontend (Vite + Svelte 5 + Tailwind)
npm run dev                 # browser preview (demo state, no Tauri)
./scripts/check-ffmpeg.sh src-tauri/binaries/ffmpeg-aarch64-apple-darwin  # G1 capability gate
./scripts/spike-render.sh   # video spike -> /tmp/spike-render.mp4
./scripts/spike-book.sh     # book spike -> /tmp/book-spike/
./scripts/make-fixture.sh   # tiny synthetic trip -> fixtures/tiny-trip/
./scripts/fetch-sidecars    # pinned FFmpeg, ffprobe, Typst -> src-tauri/binaries/
```

CLI (`triptape-media` bin `triptape` — headless, no UI):

```sh
cargo run -p triptape-media --bin triptape -- scan --folder /path/to/trip --db trip.db --cache /tmp/cache [--length auto|600|10m]
cargo run -p triptape-media --bin triptape -- plan --db trip.db --out plan.json
cargo run -p triptape-media --bin triptape -- export --db trip.db --out ~/Movies/TripTape/trip
# Bigger photo book (24–400 pages; default 60 for P1, 80 for P2):
cargo run -p triptape-media --bin triptape -- export --db trip.db --out ~/Movies/TripTape/trip --only book --pages 111
# Copy the trip's best 800 photos (with their Live Photo motion) into a new folder.
# Captioned photos come first; a shot that repeats a scene is skipped unless
# different people are in it. Add --stills-only to leave the motion out.
cargo run -p triptape-media --bin triptape -- pick --db trip.db --out ~/Pictures/trip-best --count 800
# Story edits from the command line (asset id, file name or path). --list
# shows the edits, the film's collages and its estimated length.
cargo run -p triptape-media --bin triptape -- edit --db trip.db --exclude IMG_0412.HEIC --include IMG_0420.MOV
# Collages: play a photo on its own (--unsolo undoes it), or join two
# neighbouring film photos of one day into one collage (--unjoin undoes it).
cargo run -p triptape-media --bin triptape -- edit --db trip.db --solo IMG_0899.HEIC --join IMG_1870.HEIC IMG_7072.HEIC --list
```

Optional local models (nothing is downloaded by the app or the build):

```sh
./scripts/fetch-models   # pinned + SHA-256-verified, into the app data dir's models/
```

With them installed, scans add an on-device image embedding per photo (so a
retake in another framing counts as a near-duplicate alongside pHash) and
face / smile signals that give a small, bounded score bonus ("smiling").
Without them (or with any file missing) TripTape prints one line saying what
is missing and behaves exactly as before. The CLI takes `--models DIR`; both
the CLI and the app honour `TRIPTAPE_MODELS_DIR` and `TRIPTAPE_ORT_LIB` (the
ONNX Runtime 1.31 library, whose download pin is still blocked: see
[THIRD_PARTY_MODELS.md](THIRD_PARTY_MODELS.md)).

Desktop app: `cargo run -p triptape-app` (Tauri v2 shell + Svelte frontend).
IPC: `scan_folder` / `get_assets` / `start_job` / `cancel_job` /
`export_video` / `export_reels` / `export_book`; progress over
`job://progress`; thumbnails over `asset://` scoped to the cache dir.

Docs: [docs/ffmpeg-build.md](docs/ffmpeg-build.md),
[docs/gates.md](docs/gates.md), [docs/decisions.md](docs/decisions.md),
[docs/demo-script.md](docs/demo-script.md),
[docs/submission-checklist.md](docs/submission-checklist.md).
Third-party model ledger: [THIRD_PARTY_MODELS.md](THIRD_PARTY_MODELS.md).

## Exporting from Apple Photos

[osxphotos](https://github.com/RhetTbull/osxphotos) is the recommended way to
get a trip out of Photos:

```sh
osxphotos export "<folder>" --album "<album>" --skip-original-if-edited --download-missing --update
```

- `--skip-original-if-edited` keeps your edits: the edited version is
  exported instead of the original.
- Live Photos export as a still + `.mov` and pair up automatically,
  `_edited` pairs included. Leave out `--only-photos` if you want video
  clips in the film.
- TripTape reads the `.osxphotos_export.db` osxphotos leaves in the folder
  (read-only, nothing is written next to it) for captions, who is tagged
  (stored only as a count and an anonymous signature, never names), and
  the capture date and place of edited files that lost their EXIF.
  A file's own EXIF date and GPS always win.
- XMP sidecars (`--sidecar xmp`) work too, and their captions and people
  win when both exist.

## MVP status + honest gaps

Ships: local ingest → Library grid → Story review → all three keepsakes
from one Export screen, with degradation rules (short film, 1–2 reel
packs, disabled book card) and the Reels share disclosure (no blur gate,
no extra click).

Gaps: the pinned FFmpeg for macOS is an Intel build, so Apple silicon
needs Rosetta 2 (owner decision pending, see
[docs/ffmpeg-build.md](docs/ffmpeg-build.md)); a dev run without the
sidecars uses `PATH` tools, and plain Homebrew ffmpeg has no libass, so
captions are then written as an `.srt` instead of burned in (the Export
screen says so); stabilization is not implemented yet; the sidecars add
about 200 MB (macOS) and 260 MB (Windows) before installer compression; no
signing keys or update channel (G5, updater wired off with a pubkey
placeholder); G2/G3/G4 gates open; Windows CI with the sidecars is
unverified until it runs.

## Git hooks

Install once with `npm run hooks` (needs `brew install pre-commit`). Hooks are
local and mirror CI (`.pre-commit-config.yaml`):

- **pre-commit** (fast): `cargo fmt --check` when Rust is staged; `npm run check`
  and `npm run lint` when frontend, Rust (lint only) or their config files change.
- **pre-push** (slower): `npm run build` if `dist/` is missing or stale, clippy
  with `-D warnings`, `cargo test --workspace`, then `deadwood`, `cargo deny` and
  `cargo audit` (each skipped with a notice if not installed).

In an emergency, skip with `git commit --no-verify` or `git push --no-verify`.

## License

MIT — see [LICENSE](LICENSE). FFmpeg stays a separate GPL sidecar
executable; the project license is unaffected. The bundled programs, their
licences and source links are listed in
[resources/licenses/THIRD_PARTY.md](resources/licenses/THIRD_PARTY.md),
which ships inside the app with the GPL text.

## Privacy

Local first. TripTape makes no network calls in its media pipeline and never
uploads your photos. Face detection and cloud polish are planned but have not
shipped. See `PLAN.md` Section 23 for the current state of each privacy control.

- Every export is stripped, always (no toggle): the film, reels and reel
  covers carry no tags from your files (no GPS, make/model/lens, serials,
  capture times or Live Photo `ContentIdentifier`), and book pages, the
  cover and the PDFs are re-encoded with no EXIF. Rotation is applied to
  the pixels, so films play upright. `export.json`, `hero.edl.json` and
  `GUIDE.txt` hold no coordinates or people's names.
- Hide home on maps: set home in Settings (latitude/longitude, or where the
  trip starts or ends; no geocoding). Route points within 500 m of home are
  moved out to 500 m on the film's map and the book's map page. The CLI
  uses `--home LAT,LON`, else the home saved in the app.
- A rolling local log (5 files of 4 MB, 7 days) is redacted as it is
  written: your home folder becomes `~`, other folder and file names become
  salted short hashes, coordinates become `<gps>`, and the trip name,
  captions and place names become `<text>`. Settings > Copy diagnostic
  report puts app, system, tool, cache and recent log facts on the
  clipboard (or saves a file when there is no clipboard). Nothing is sent.

## Clean-machine notes

- macOS: `brew install rust exiftool`, then `./scripts/fetch-sidecars`
  (and `softwareupdate --install-rosetta` on Apple silicon); build with
  cargo. No Xcode license, kernel extensions, or daemons required.
- Windows: install Rust, then run `./scripts/fetch-sidecars` from Git Bash.
- Tool lookup: `TRIPTAPE_FFMPEG`/`TRIPTAPE_FFPROBE`/`TRIPTAPE_TYPST` (or the
  CLI's `--ffmpeg`/`--ffprobe`/`--typst`), then the bundled sidecar next to
  the executable, then `PATH` (dev only, noted).
- Thumbnails and film pieces cache in the OS cache dir. Settings shows both
  sizes, clears each after a confirmation (only inside the app's cache
  folder), and sets the film-piece limit (CLI: `--segment-cache-max`, else
  the app's limit). Thumbnails have no cap. Exports render in `.work`
  inside the export folder, which is removed when the export ends.

## Known blockers (Sprint 0)

- Sidecars pinned (FFmpeg 9.0.2, Typst 0.15.1); G1 still needs Windows CI,
  a no-GPU baseline encode and playback checks.
- G2 vendor workflow, G3 offline maps, G4 model budgets, G5 signed update:
  all open — see evidence in [docs/gates.md](docs/gates.md).
