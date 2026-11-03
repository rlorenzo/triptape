# Agent Review Summary

## Overview

The review loop ran 3 cycles over the Sprint 0 foundations scaffold. It began with a refinement pass that simplified code, and then worked through review findings until the reviewer reported none. Most of the work landed in the ingest core (`triptape-core`). Live Photo pairing is now stricter. Metadata extraction handles real-world rotation, orientation, time zones and memory limits. Proxy/thumbnail caching is now atomic. Folder scans report failures instead of dropping files silently. Frame scoring is bounds-safe. The loop also tidied the helper shell scripts and removed unused dependencies. Each fix came with a regression test.

## Improvements Made

- **Live Photo pairing no longer merges unrelated files.** Pairing by matching file stem (tier 2) now requires the still and the motion file to sit in the same directory. It also rejects a pair when both files carry different content identifiers. Before this, `trip1/IMG_106.HEIC` could be paired with `trip2/IMG_106.MOV`, and files with explicitly conflicting Apple IDs could be merged. A missing ID on one side is still allowed.
- **Video rotation reads modern ffprobe output.** Current ffprobe reports phone-video rotation in `Display Matrix` side data, which is counter-clockwise, rather than in the legacy `rotate` tag. Rotation now prefers the display matrix, with the sign corrected, and falls back to the legacy tag. Before, most modern iPhone/Android clips would have been reported as unrotated.
- **All eight EXIF orientations are modeled.** A new `OrientationTransform` carries a horizontal mirror flag plus a clockwise rotation, so mirrored orientations 2, 4, 5 and 7 can render correctly. `orientation_to_degrees` remains as the rotation-only view, and its doc comment warns that this view loses the mirror.
- **Capture times respect time-zone offsets.** EXIF `OffsetTimeOriginal` is now combined with `DateTimeOriginal`, and offset-bearing strings are parsed first, so the result is an exact instant. The old code parsed `%#z` through `NaiveDateTime` and dropped the offset. Naive wall-clock times are still encoded as if UTC, and the code documents this as a known gap until trip-timezone normalization lands.
- **ISO 6709 parsing accepts altitude.** Apple location strings such as `+37.7749-122.4194+15.0/` include altitude. The old parser split on the last sign character and so rejected them or misparsed them. Fields are now split by sign position, and the altitude is ignored.
- **EXIF reads have a memory limit.** The `rexif` fallback used to read the whole file into memory. Non-TIFF containers now stream through a buffered reader. TIFF-based files (most RAW formats) and the fallback parse a prefix capped at 16 MiB (`EXIF_READ_CAP`), so a large or hostile file can't force a whole-file allocation.
- **The EXIF API is clearer.** The `(Option, Option, Option)` tuple was replaced by an `ImageExif` struct. GPS parsing for the two EXIF libraries was duplicated; it now goes through shared `dms_to_deg`, `signed_gps` and `valid_lat_lon` helpers.
- **The proxy cache can't be poisoned by partial output.** FFmpeg now writes to a unique temporary file next to the target. That file is renamed into place only after a successful exit with non-empty output, and it is deleted on failure. Before, a crashed or interrupted render left a truncated file at the cache path, which later runs treated as a cache hit. `run_ffmpeg` also checks that the output argument matches the cache path.
- **Thumbnails never upscale.** The scale filter now caps the long edge with `min(iw,320)` / `min(ih,320)`. The previous filter enlarged small images even though its comment said it only downsized.
- **Scans report partial results.** Directory-walk errors, such as unreadable subdirectories, were silently discarded by `filter_map(|e| e.ok())`. They are now collected in `ScanResult.errors` along with their paths, and `is_partial()` gives the UI a way to warn. Supported media whose metadata or mtime can't be read is also reported as an error. Before, such files were skipped silently.
- **Pre-1970 timestamps are kept.** Files with mtimes before 1970 (bad camera clocks, restored archives) used to be dropped. Their mtimes are now stored as negative seconds. Scanning also reuses the metadata walkdir already loaded instead of stat-ing each file a second time.
- **Frame scoring is bounds-safe.** `frame_stats` uses a checked `width * height`, so huge dimensions can't overflow. It also computes statistics only over the declared frame area, so trailing buffer padding no longer skews the mean and variance.
- **Script and dependency cleanup.** `fetch-ffmpeg` now resolves the URL and hash from one platform table instead of two `case` blocks that could drift apart. `spike-render.sh` makes one ffprobe call for width and height and uses `awk` instead of an embedded `python3`. `check-ffmpeg.sh` replaces the `&& ||` idiom with a proper `if`. Unused `serde`, `thiserror` and `rusqlite` dependencies were removed. BLAKE3 hashing uses `update_reader`.
- **Tests added.** New tests cover conflicting IDs and directory scoping in Live Photo pairing; display-matrix rotation; ffprobe `N/A` fallbacks; all EXIF orientations; offset-aware datetimes; ISO 6709 altitude; DMS/hemisphere signing; non-EXIF and missing files; FFmpeg argument order; cache-hit semantics; temp-file cleanup after a failed FFmpeg run; unreadable-directory reporting; hidden and system file skipping; and padded-frame statistics.

## Created Files

None.

## Remaining Items

None blocking. The final review reported no findings. One documented limitation is still open: EXIF capture times without an offset are stored as UTC wall-clock time, pending trip-timezone normalization.

## Files Changed

- scripts/check-ffmpeg.sh
- scripts/fetch-ffmpeg
- scripts/spike-render.sh
- src-tauri/crates/triptape-ai-local/Cargo.toml
- src-tauri/crates/triptape-ai-local/src/lib.rs
- src-tauri/crates/triptape-core/Cargo.toml
- src-tauri/crates/triptape-core/src/hash.rs
- src-tauri/crates/triptape-core/src/livephoto.rs
- src-tauri/crates/triptape-core/src/metadata.rs
- src-tauri/crates/triptape-core/src/proxies.rs
- src-tauri/crates/triptape-core/src/scan.rs
- src-tauri/crates/triptape-core/src/score.rs

## Metrics

- Cycles: 3 (plus an initial refinement pass)
- Duration: 14m 59s
- Initial review: High 4, Medium 2, Low 0
- Final: High 0, Medium 0, Low 0 (CLEAN)
- Net diff growth from review: 1,936 → 2,271 lines added (+335)
