# TripTape: notes for coding agents

TripTape turns a trip's photos and videos into a film, reels and a photo book. The CLI is `triptape` (`cargo run -p triptape-media --bin triptape -- <scan|plan|export|edit|pick|learn> ...`); the desktop app is `cargo run -p triptape-app`. See `README.md` for the full command list and `docs/` for decisions and gates.

## Build and check

```sh
npm install && npm run build       # frontend first: Tauri embeds dist/, so cargo fails without it
cargo build --workspace
cargo test --workspace
./scripts/check-ffmpeg.sh <ffmpeg> # capability gate
```

CI also fails on `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `npm run lint` and `npm run check`. `npm run hooks` installs pre-commit and pre-push hooks that run them.

Plain Homebrew `ffmpeg` lacks libass, drawtext, zscale and vidstab. For real film renders pass `--ffmpeg /opt/homebrew/opt/ffmpeg-full/bin/ffmpeg --ffprobe /opt/homebrew/opt/ffmpeg-full/bin/ffprobe`. In your own ffmpeg commands, ffmpeg-full rejects `-vsync`; use `-fps_mode passthrough`.

## How work lands

- One PR per change. Don't merge until `code-review-loop --branch origin/main` is clean and CI is green, and merge with `gh pr merge`, never locally.
- Work in a git worktree, not the main checkout, which other sessions use.
- Touching a line that spawns ffmpeg can re-raise CodeQL `rust/command-line-injection`; expect to triage it.

## Owner edits are sacred

- `learn --apply` and a rescan re-plan the trip and drop shots the owner kept. Never run either on a project the owner has edited; use `triptape edit`.
- `edit` drops the whole call if any one flag is refused (for example `--include` of an asset that is in no plan chapter). Re-check with `--list` after every edit.
- A clip window lives in two places: `assets.score_json.clip_windows` and the stored plan's hero `rendition.clip`. Change both.
- Back up `trip.db` before a batch of edits.

## Rendering the film: what the iterations taught us

- **Measure where the time goes before optimizing the encoder.** With `--work-dir` and `--segment-cache` on a network share reached over Wi-Fi, a draft that reused 213 of 217 cached segments still took over 4 hours. On a local SSD the same draft took about 10 minutes. Keep the work dir and segment cache on fast local storage; originals can stay on the NAS.
- **A new binary invalidates the whole segment cache.** The next render re-encodes everything. Batch code changes during an owner review cycle.
- **A missing source share is not an error today.** If the volume holding the originals is unmounted, export marks every asset "missing", skips them, exits 0 within seconds and writes no `hero*.edl.json`. Wrapper scripts must check the share is mounted. Making export fail when most assets are missing would be a good fix.
- **Any length change needs a new music bed.** Soloing a collage, changing a clip window or adding a shot changes the total, and the EDL then reports music seams and `ends_naturally: false`. The plan's length estimate can be off by ~30 s on a 15-minute film, so fit the bed to the measured `total_secs` from the EDL and re-render (cheap, since segments are cached). Ship only `seams: []` and `ends_naturally: true`.
- **Collage strips are the most common visual defect.** A landscape photo in a narrow portrait strip letterboxes with blur bars, and a group photo in a narrow strip slices faces. Owner fixes are `edit --frame` (percent of the upright photo) or `--solo`; a layout-level fix should weigh tile aspect against photo aspect.
- **Look at real frames.** Extract frames from the output (decode sequentially; don't seek before `-i` on HEVC) and read HEIC originals with `sips`, since ffmpeg decodes only one tile of a HEIC. Contact-sheet thumbnails are too small to judge crops or captions over faces.
- **Clip windows often stop mid-action.** When choosing or reviewing a window, scan the whole source (a 2 fps tile sheet is enough) and let the action finish before the cut.
- **Delivery format:** the 4K film is HEVC Main 10 `hvc1`, VBV-capped, about 21 Mbit/s on average. It must stream through Plex to a Roku 4K, so check that the busiest second stays well under 100 Mbit/s.

## Shared state

- `itinerary.json` (owner place names) is shared by the film and the photo book. A rename reaches both on their next render. Back it up before editing, and tell any other session working on the same trip.
- Several agent sessions may share one machine and NAS. Kill your own processes by PID, never with `pkill <name>`.
