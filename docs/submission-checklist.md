# Submission Checklist (PLAN.md Section 18)

## Product

- [ ] TripTape desktop app (macOS universal + Windows x64) with all three keepsakes.
- [ ] Demo fixture trip (synthetic, redistributable — `scripts/make-fixture.sh`).
- [ ] Sample exports from the fixture: hero MP4, reels folder, book proof PDF + JPEG pack.

## Repo

- [x] `PLAN.md` (source of truth), `LICENSE`, `THIRD_PARTY_MODELS.md`.
- [x] `README.md` with build/run/test, CLI usage, MVP status + honest gaps.
- [x] CI (`.github/workflows/ci.yml`) — `cargo test --workspace`, FFmpeg capability gate, `npm run build`.
- [ ] CI passing on **both** macOS and Windows (not yet run on Windows).

## Hackathon

- [ ] Project description naming the "Build for Your World" track + family problem.
- [ ] Demo video (live flow + fallback capture) under the track time limit.
- [ ] Privacy + licensing statement: local-first faces, Meta OSS usage, third-party model licenses.

## Known blockers (honest gaps)

- Sidecar SHA-256 pins unfilled (`scripts/fetch-ffmpeg` refuses to install; G1 open).
- No signing keys / test update channel (G5 open — updater plugin configured `active: false` with a minisign pubkey placeholder in `src-tauri/tauri.conf.json`).
- G2/G3/G4 gates open — see [`docs/gates.md`](gates.md).
