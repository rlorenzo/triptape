# Agent Review Summary

## Overview

The review loop added no review-driven fixes. The reviewer found no High, Medium, or Low issues. Before that review, a code-refinement pass cleaned up the osxphotos export-database feature. It moved the photo and video metadata-filling logic, which had been copied into both the CLI and the Tauri app, into one shared core implementation. It also made the database loader do less redundant work, replaced a hand-rolled timestamp-offset parser with jiff's structured parser, and removed public API that only tests used. The feature's behavior did not change, and a new test covers the shared path end to end.

## Improvements Made

- **One metadata-fill path for the CLI and the app.** The CLI (through a `StoryCtx` helper) and the Tauri app (through an inline closure) each had their own copy of the same video logic: duration, dimensions, rotation, GPS, capture time, and the mtime fallback for videos ffprobe can't read. The still logic was duplicated the same way: EXIF capture time with library fallback, story fields, rotation, and EXIF pixel size. The refinement replaced both copies with core `fill_photo` and `fill_video` functions, so the two ingest paths can no longer drift apart. The CLI still applies decoded-pixel dimensions and a pHash on top when an image decodes.
- **More robust ISO 8601 offset parsing.** `parse_iso_instant` used to find the UTC offset by scanning for the last `+`/`-` after character 10, then parsing that substring by hand. It now uses jiff's `Pieces` parser, which reads `Z` and numeric offsets directly and rejects anything else. That removes a string-slicing heuristic that could break on unusual input.
- **Each photo's JSON is parsed once, not once per file.** One library photo can export as several files: a still plus a Live Photo motion clip, or an original plus an edit. The loader now caches parsed `photoinfo` by uuid, so it parses each photo's JSON once instead of once per exported file.
- **The folder is resolved once.** `OsxPhotosIndex::load` now canonicalizes the folder once and reuses the result to locate the database, open it, and build its lookup roots. Previously it canonicalized in two places. `open_immutable` no longer falls back silently to a non-absolute path.
- **Smaller public API.** `xmp_people_count`, `OsxPhotosIndex::len` and `OsxPhotosIndex::is_empty` were public only so tests could call them. They are now test-local helpers or direct field access in tests.
- **Small idiom fixes.** Percent-encoding writes into the output buffer with `write!` instead of building a temporary string for every byte. The BLAKE3 digest prefix converts with `try_into`. Splitting tagged people into named and unnamed now uses two filters instead of building two intermediate vectors.
- **New test.** A new test exercises `fill_photo` and `fill_video` against a real osxphotos database. It checks that an edited still gets its capture time, GPS, caption, people count, rotation, and dimensions from the library. It also checks that a video ffprobe can't read falls back to mtime, still gets its story fields, and gets the same people signature as its paired still.

## Created Files

None. (`agent-code-review.md`, the reviewer's report, is untracked; it is not a source file.)

## Remaining Items

None. The final review reported no findings and gave a "good to go" verdict.

## Files Changed

- `src-tauri/crates/triptape-core/src/metadata.rs`
- `src-tauri/crates/triptape-core/src/osxphotos.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`
- `src-tauri/src/lib.rs`

## Metrics

- Review cycles: 0 (one refinement pass, then one clean review)
- Duration: 22m 31s
- Final issue counts: High 0, Medium 0, Low 0
- Status: CLEAN
- Refinement diff: 4 files, +144 / −146 lines
