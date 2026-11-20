# Sidecar binaries (not in git)

`scripts/fetch-sidecars` puts the pinned FFmpeg, ffprobe and Typst
executables here as `<name>-<target-triple>[.exe]`, the names Tauri's
`bundle.externalBin` expects. Every file is checked against a pinned SHA-256.
Versions, sources and licences: `docs/ffmpeg-build.md`.

Without them the app still builds: `src-tauri/build.rs` drops
`externalBin` from the build and the app falls back to `ffmpeg`, `ffprobe`
and `typst` on `PATH` (dev only, with a visible note). Release bundles need
them.
