# TripTape — Drop in the trip. Get the story back.

Free, open-source desktop app (Tauri v2 + Rust) that turns a messy folder of
family vacation photos/videos into three keepsakes from one curation pass:
a Trip Video (1080p hero film), a Reels Pack (3–5 vertical clips), and a
Photo Book (print-ready PDF + per-page JPEGs). See [PLAN.md](PLAN.md) —
the source of truth until code exists.

## Run (Sprint 0)

Prerequisites: Rust toolchain, system `ffmpeg`/`ffprobe` 9.x, `typst`
0.15.1, `exiftool` 13.55. No signing keys or vendor accounts needed.

```sh
cargo build --workspace     # all Rust crates
cargo test --workspace      # 24 tests, all in triptape-core
./scripts/check-ffmpeg.sh   # G1 capability gate (warns on known gaps)
./scripts/spike-render.sh   # video spike -> /tmp/spike-render.mp4
./scripts/spike-book.sh     # book spike -> /tmp/book-spike/
./scripts/fetch-ffmpeg      # sidecar pin (BLOCKED: SHA-256 placeholders)
```

Docs: [docs/ffmpeg-build.md](docs/ffmpeg-build.md),
[docs/gates.md](docs/gates.md), [docs/decisions.md](docs/decisions.md).
Third-party model ledger: [THIRD_PARTY_MODELS.md](THIRD_PARTY_MODELS.md).

## License

MIT — see [LICENSE](LICENSE). FFmpeg stays a separate GPL sidecar
executable; the project license is unaffected.

## Privacy

Local first, cloud by consent. Faces are detected on-device (count + boxes
only; naming is opt-in), originals are never uploaded, and face data is
deletable in Settings. Cloud polish is a per-trip opt-in, never default.

## Clean-machine notes

- macOS: `brew install rust ffmpeg typst exiftool`; build with cargo.
  No Xcode license, kernel extensions, or daemons required.
- Windows: install Rust + the pinned static FFmpeg sidecar (see
  `scripts/fetch-ffmpeg`); PATH fallback is dev-only.
- Cache lives in the OS cache dir (content-addressed, 5–10 GB LRU);
  temp work is job-scoped under the temp dir and deleted on completion.

## Known blockers (Sprint 0)

- Sidecar SHA-256 pins unfilled (`scripts/fetch-ffmpeg` refuses to install).
- System ffmpeg lacks `zscale`/`drawtext`/`vidstab`/`libass` (recorded G1 gaps).
- G2 vendor workflow, G3 offline maps, G4 model budgets, G5 signed update:
  all open — see evidence in [docs/gates.md](docs/gates.md).
