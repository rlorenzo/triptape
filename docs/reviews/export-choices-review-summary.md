# Agent Review Summary

## Overview

The review loop ran one refinement pass and one review cycle on top of the export-choices work (keepsake eligibility, book presets, destination picking, source checks, and Relink). The refinement pass removed duplicated helpers, made relink matching cheaper and stricter, cut unnecessary reloads and hashing, and consolidated repeated UI and dialog code. The review then found one medium-severity correctness gap: Relink updated the asset rows but not the snapshots stored in the plan's film and reel picks, so a relinked original could still fail the pre-run check. That was fixed atomically and covered by a test. The final review reported no open issues.

## Improvements Made

- **Relink now updates the plan as well as the assets (the medium finding).** The pre-run check and the export compare the film (`hero`) and reel (`reel_selects`) picks against size/mtime/hash snapshots stored in the plan. Relink only rewrote the asset rows, so a moved file with a new mtime was still flagged as "changed", and a pick with no hash had to be re-hashed every time. A new `relink_plan` copies the relinked rows' size and mtime into the matching plan entries and fills in a missing hash. It runs inside the same transaction as the asset updates and saves a new plan only when something changed. A unit test covers an unhashed pick whose mtime changed after the move: it fails the check before `relink_plan` runs and passes after.
- **Same-size files with the same name can no longer be linked to the wrong photo.** Candidates are now filtered by size before hashing in both branches, so the hash is checked only on plausible files and is still what tells same-size lookalikes apart. The relink test gained a same-named, same-size file with different bytes to prove it is rejected.
- **Relink no longer hashes everything to find what's missing.** It used to run a full source check, which could hash files, just to get the list of missing ids. It now walks the export sources and stats each path.
- **One source of truth for book-flow and snapshot logic.** The app crate had its own copies of the book-chapter filter, the asset-snapshot builder, and the size/mtime reader. These were replaced by the media crate's existing functions, which are now `pub`, so the eligibility check and the real export can't disagree about which photos the book uses or how mtimes are read.
- **Fewer redundant reloads on the Export screen.** Eligibility, folder, and interrupted-export loads now react only to the project id, not to every project-store update.
- **Simpler UI code.** The run-or-show-the-sheet decision, which had been written twice, became one `runOrAsk` helper, and the missing/changed file lists share one `fileList` snippet. Import uses the shared `pickFolder` helper instead of its own copy of the dialog call. A redundant `destination` local was removed in the demo exporter.

## Created Files

None.

## Reverted After Review

The refinement pass made four private helpers in `triptape-media/src/export.rs` public so the app could reuse them. That file is out of scope for this branch (another agent is editing it), so the change was reverted and the app keeps its own mirrors (`row_snapshot`, `size_mtime`, and the chapter filter in `export_sources`), each commented with the export function it mirrors.

## Remaining Items

None. The final review reported no findings.

## Files Changed

- `src-tauri/src/lib.rs`
- `src-tauri/src/main.rs`
- `src/Export.svelte`
- `src/Import.svelte`
- `src/lib/tauri.ts`

## Metrics

- Cycles run: 1 (plus an initial refinement pass)
- Duration: 11m 23s
- Initial review: High 0, Medium 1, Low 0
- Final review: High 0, Medium 0, Low 0 (CLEAN)
