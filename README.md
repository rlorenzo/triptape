# TripTape — Drop in the trip. Get the story back.

Free, open-source desktop app (Tauri v2 + Rust) that turns a messy folder of
family vacation photos/videos into three keepsakes from one curation pass:
a Trip Video (4K hero film, 1080p optional via `export --resolution 1080p`), a Reels Pack (3–5 vertical clips), and a
Photo Book (print-ready PDF + per-page JPEGs). See [PLAN.md](PLAN.md) —
the source of truth until code exists.

## Run (Sprint 6)

Prerequisites: Rust toolchain, system `ffmpeg`/`ffprobe` 9.x, `typst`
0.15.1, `exiftool` 13.55, Node 20 + npm. No signing keys or vendor
accounts needed.

```sh
cargo build --workspace     # all Rust crates incl. Tauri glue
cargo test --workspace      # core + media + glue unit tests
npm install && npm run build  # frontend (Vite + Svelte 5 + Tailwind)
npm run dev                 # browser preview (demo state, no Tauri)
./scripts/check-ffmpeg.sh   # G1 capability gate (warns on known gaps)
./scripts/spike-render.sh   # video spike -> /tmp/spike-render.mp4
./scripts/spike-book.sh     # book spike -> /tmp/book-spike/
./scripts/make-fixture.sh   # tiny synthetic trip -> fixtures/tiny-trip/
./scripts/fetch-ffmpeg      # sidecar pin (BLOCKED: SHA-256 placeholders)
```

CLI (`triptape-media` bin `triptape` — headless, no UI):

```sh
cargo run -p triptape-media --bin triptape -- scan --folder /path/to/trip --db trip.db --cache /tmp/cache [--length auto|600|10m]
cargo run -p triptape-media --bin triptape -- plan --db trip.db --out plan.json
cargo run -p triptape-media --bin triptape -- export --db trip.db --out ~/Movies/TripTape/trip
```

Desktop app: `cargo run -p triptape-app` (Tauri v2 shell + Svelte frontend).
IPC: `scan_folder` / `get_assets` / `start_job` / `cancel_job` /
`export_video` / `export_reels` / `export_book`; progress over
`job://progress`; thumbnails over `asset://` scoped to the cache dir.

Docs: [docs/ffmpeg-build.md](docs/ffmpeg-build.md),
[docs/gates.md](docs/gates.md), [docs/decisions.md](docs/decisions.md),
[docs/demo-script.md](docs/demo-script.md),
[docs/submission-checklist.md](docs/submission-checklist.md).
Third-party model ledger: [THIRD_PARTY_MODELS.md](THIRD_PARTY_MODELS.md).

## MVP status + honest gaps

Ships: local ingest → Library grid → Story review → all three keepsakes
from one Export screen, with degradation rules (short film, 1–2 reel
packs, disabled book card) and the Reels share disclosure (no blur gate,
no extra click).

Gaps: FFmpeg sidecar SHA-256 pins unfilled (system ffmpeg in dev only,
`zscale`/`vidstab`/libass (`subtitles`) missing on plain Homebrew ffmpeg — hero captions burn (needs libass) and
stabilization wait on the sidecar); no signing keys or update channel
(G5 — updater wired off with a pubkey placeholder); G2/G3/G4 gates open;
CI not yet green on Windows.

## License

MIT — see [LICENSE](LICENSE). FFmpeg stays a separate GPL sidecar
executable; the project license is unaffected.

## Privacy

Local first. TripTape makes no network calls in its media pipeline and never
uploads your photos. Face detection and cloud polish are planned but have not
shipped, and exports do not strip EXIF yet. See `PLAN.md` Section 23 for the
current state of each privacy control.

## Clean-machine notes

- macOS: `brew install rust ffmpeg typst exiftool`; build with cargo.
  No Xcode license, kernel extensions, or daemons required.
- Windows: install Rust + the pinned static FFmpeg sidecar (see
  `scripts/fetch-ffmpeg`); PATH fallback is dev-only.
- Thumbnails cache in the OS cache dir with no size cap yet. Exports render
  in `.work` inside the export folder, which is removed when the export ends.

## Known blockers (Sprint 0)

- Sidecar SHA-256 pins unfilled (`scripts/fetch-ffmpeg` refuses to install).
- System ffmpeg lacks `zscale`/`drawtext`/`vidstab`/libass `subtitles` (recorded G1 gaps).
- G2 vendor workflow, G3 offline maps, G4 model budgets, G5 signed update:
  all open — see evidence in [docs/gates.md](docs/gates.md).
