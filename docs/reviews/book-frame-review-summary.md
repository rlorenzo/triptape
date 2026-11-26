# Agent Review Summary

## Overview

The original change let the owner's `--frame` rectangle cut a book photo as well as a film shot. The review loop left that feature's shape alone and worked on its edges. A refinement pass removed a second copy of the percent-to-pixel frame arithmetic, so the film and the book now cut a framed photo with the same code, and moved frame validation to the one place that applies a frame. The reviewer then raised two medium findings, both cases where a framing was silently lost or misapplied in the book, and both were fixed with tests in a single cycle. The final review found nothing.

## Improvements Made

- **A framing on a book-only photo is no longer dropped.** Applying the owner's edits kept a frame only for photos in the film plan's chapters. A photo the plan had set aside but the owner pinned into the book lost its framing before the book was built, so the page printed the whole picture, including whatever the owner had framed out. Frames are now kept for any photo the film or the book can show, are still dropped for excluded photos, and are still dropped for photos neither output uses. A test covers both the pinned and the unused case.
- **The cover tile's focus follows the frame.** Framing a photo moved its size, family boxes and crop point into the framed picture's terms but left the cover focus in whole-picture terms, so a framed cover photo could be centred on the wrong part. An explicit cover focus is now mapped into the framed picture's horizontal range. A centred pin (0.5), which means "no focus", is passed through untouched so it keeps that meaning. A test covers both.
- **One frame-to-pixels calculation for the film and the book.** The book had its own copy of the rounding and clamping that turns a percent rectangle into pixels, next to a slightly different one in the film renderer. The calculation now lives once in the core types, and both renderers call it, so the two outputs cannot disagree about where a frame's edges fall. The book's separate crop helper was removed in favour of the film's.
- **Frame validation happens where the frame is applied.** The export step filtered out invalid rectangles before handing them to the book photo. That check moved into the framing step itself, so an unusable rectangle (empty, or running past the edge) leaves the photo unchanged whoever the caller is. This also let the fraction helper lose its defensive clamps, since it now only ever sees valid frames. A test covers the unusable-rectangle case.

## Created Files

None.

## Remaining Items

None. Both medium findings were resolved and the final review reported no open issues.

## Files Changed

- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-core/src/types.rs`
- `src-tauri/crates/triptape-media/src/export.rs`
- `src-tauri/crates/triptape-media/src/render_book.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`

## Metrics

- Cycles run: 1 (after a code refinement pass and the initial review)
- Duration: 18m 51s
- Initial review: High 0, Medium 2, Low 0
- Final review: High 0, Medium 0, Low 0
- Final status: CLEAN, all issues resolved
