# Agent Review Summary: Sprint 3-5 Renderers

## Overview

The review loop took the first version of the export pipeline (video, reels, and book renderers plus the `triptape export` CLI) and fixed several problems where an export could finish without errors but give the user a wrong or unsafe result. Exports now check every source file against the plan before rendering it. Rendered maps get the same home-location fuzzing as plan JSON. Clips with no audio no longer break the encode. The 30-second short video respects its time limit. Book captions now appear in the JPEGs that print vendors actually use. A refinement pass also moved repeated FFmpeg setup code into shared helpers and tidied the code. The loop finished clean after two cycles.

## Improvements Made

- **Source integrity before rendering.** Export now compares each original file's size and modification time to the plan snapshot, and hashes the file only when those differ. Missing or changed files are skipped, and the reason goes into the fallbacks report. Before this, the export could render photos that had been edited or swapped since planning. The check covers video entries, reel entries, and book photos. Book photos also get a cheap header read for their dimensions, and unreadable images are reported and skipped instead of causing a failure partway through.
- **Home privacy for rendered maps.** `--home LAT,LON` now fuzzes the route polyline before the map is drawn, which matches the privacy rule already applied to plan JSON. Before this, the exact home location could show up in the map segment of the video and on the book's map page.
- **Plan lookup consolidated.** `latest_plan` now calls the project-aware lookup instead of keeping its own copy of the query. A new test checks that a lookup across all projects returns the project that owns the plan.
- **Free-space precheck no longer counts headroom twice.** The CLI applied the 1.5x headroom and then the precheck applied it again, which could block exports on disks that had enough space. The CLI now passes the raw estimate.
- **Silent sources no longer break encodes.** Muted or camera-silent clips get a generated silent stereo track, in both the hero video and the reels. Before this, the video's audio crossfade step and the output check (which requires an audio stream) failed on these sources. Each one is noted in fallbacks.
- **Short-video time limit enforced.** The ~30 s budget now subtracts the title and map segments first, and each clip is trimmed to the time left before it is encoded. Before this, the check only ran after a segment was rendered, so the last clip could push the short video well over 30 s.
- **Book: captions reach the print files.** Vendors print the page JPEGs, not the PDFs, and captions had only been in the PDFs. After `press.pdf` is written (so neither PDF gets the caption twice), the captioned proof pages are rasterized at 300 ppi with Typst, their size is checked, and they replace the matching page JPEGs. If Typst is missing or this step fails, the problem is added to fallbacks and the vendor guide shows a clear warning that the JPEGs have no captions. A release-mode end-to-end test, ignored by default, checks that caption pixels end up in the right JPEG and nowhere else.
- **Book: fail early on page-count limits.** Page-cap and minimum-page blocks are now checked from the page flow before any image processing starts, so a book that is too long or too short fails in seconds instead of after rendering every page.
- **Shared FFmpeg helpers and cleanup.** The libx264/AAC encode arguments, the loudnorm target, and path-to-argument conversion are now shared helpers used by every renderer, with a unit test for the encode arguments. The loop also removed the unused `NotImplemented` error, simplified the capability-probe code, and fixed clippy findings (`&[..]` instead of `&vec![..]`, formatting).

## Created Files

None.

## Remaining Items

None. The final review reported no open issues. The end-to-end caption test is `#[ignore]` because full-size pages are slow in debug builds. Run it with `cargo test -p triptape-media --release -- --ignored`.

## Files Changed

- src-tauri/crates/triptape-ai-local/src/lib.rs
- src-tauri/crates/triptape-core/src/db.rs
- src-tauri/crates/triptape-media/src/bin/triptape.rs
- src-tauri/crates/triptape-media/src/export.rs
- src-tauri/crates/triptape-media/src/ffmpeg.rs
- src-tauri/crates/triptape-media/src/lib.rs
- src-tauri/crates/triptape-media/src/map.rs
- src-tauri/crates/triptape-media/src/render_book.rs
- src-tauri/crates/triptape-media/src/render_reels.rs
- src-tauri/crates/triptape-media/src/render_video.rs
- src-tauri/crates/triptape-media/src/titlecard.rs

## Metrics

- Cycles: 2 (after a code-refinement pass and the initial review)
- Duration: 18m 56s
- Issues: initial review found High 5, Medium 2. Cycle 1 found High 1. Final: High 0, Medium 0, Low 0
- Final status: CLEAN
