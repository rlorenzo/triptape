# Code Review Summary

## Overview

The review loop kept the day, location, and timezone feature as designed and fixed places where it produced wrong output. It corrected timezone inference for photos with no recorded offset, tied photo book dividers to real chapter boundaries, and made print-resolution checks cover divider backgrounds. It also made the database migration report failures, kept short-mode videos within their time budget, and removed duplicated code. Reported issues fell from 2 High and 2 Medium to none.

## Improvements Made

**Correctness**

- **Timezone inference for naive timestamps:** Photos without an offset store local wall-clock time, but they were being compared directly with true UTC instants. On a multi-timezone trip, a photo could inherit the wrong offset and land on the wrong day. Each candidate offset is now tested by normalizing the time and comparing it with assets that carry that offset. Videos, which record true instants, still use a direct comparison.
- **Book dividers follow real chapters:** Dividers were evenly spaced and titled by position, so a title could sit on another chapter's photo, and short chapters could lose their title. Page planning now works from each photo's chapter, and a divider opens a chapter at its first remaining photo.
- **Chapter titles appear once:** No photo page crosses a divider. The hero and any chapter used up by the spread carry their own titles, and chapters with no placed photos get no divider. Trips with more than six eligible chapters have their dividers spread evenly across the trip.
- **Captions on title pages:** Automatic datelines and place captions that would repeat a chapter title are dropped, while user-written captions stay. A divider's background photo also keeps its own caption.
- **Page fitting uses the actual selection:** Page count depends on which chapters survive photo selection, so each candidate selection is now flowed for real. Fit checks, DPI checks, and final rendering share the same page plan.
- **Divider print quality:** Divider backgrounds print full-page but were left out of DPI planning and preflight, so an undersized image could reach the print proof. They are now blocked during planning and included in preflight.
- **Short-mode budget:** Freeze-padding could extend a short clip to the 2.5-second minimum even when less budget remained. The pad target is now limited by the remaining budget.

**Reliability and performance**

- **Database migration:** The `tz_offset` migration ignored every `ALTER TABLE` error, which could hide a real failure. It now checks whether the column exists and reports any failure, and it is safe to run repeatedly.
- **Chapter location lookup:** The medoid search is quadratic, so very large chapters could slow plan building. It now uses an even sample of at most 256 GPS fixes.
- **Script cleanup:** The gazetteer script now removes its temporary directory on exit.

**Simplification and accuracy of docs**

- **Shared helpers:** PNG overlay burning, photo consumption order, chapter headings, local-date lookup, and wide-photo detection each have one implementation instead of several copies. Book titles and video overlays now use the same chapter wording, including the fallback for older saved plans.
- **Smaller code paths:** The review loop removed an unused date formatter, a redundant map drawing helper, and repeated `None` fields in chapter construction. It also reduced the visibility of a constant that didn't need to be public.
- **Comments:** Comments that still described old behavior, such as "centroid" for what is now a medoid and outdated caption formats, were corrected.

**Test coverage**

New tests cover:

- Migrating an older database and rerunning the migration
- Per-offset timezone inference on a Japan and Hawaii trip
- Medoid sampling on large chapters
- Clip padding limits
- Burning a reel overlay in place
- Divider placement, including uneven chapters, chapters with no placed photos, and the divider limit
- Caption deduplication
- DPI blocking for divider backgrounds
- Chapter titles reaching the correct pages end to end

## Created Files

None.

## Remaining Items

The final review had no open findings. These known limits remain:

- `fitting_count` can settle slightly below the largest possible selection because fit isn't monotonic. Its result still fits the page cap.
- Trips with more than six eligible chapters don't get a divider for every chapter.
- The end-to-end chapter title test is marked `#[ignore]` because it is slow in debug builds, so a default test run skips it.

## Files Changed

- `scripts/make-gazetteer.sh`
- `src-tauri/crates/triptape-ai-local/src/lib.rs`
- `src-tauri/crates/triptape-core/src/db.rs`
- `src-tauri/crates/triptape-core/src/geo.rs`
- `src-tauri/crates/triptape-core/src/metadata.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-core/src/types.rs`
- `src-tauri/crates/triptape-media/src/export.rs`
- `src-tauri/crates/triptape-media/src/map.rs`
- `src-tauri/crates/triptape-media/src/render_book.rs`
- `src-tauri/crates/triptape-media/src/render_reels.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`
- `src-tauri/crates/triptape-media/src/titlecard.rs`

## Metrics

- **Cycles run:** 3
- **Duration:** 21m 31s
- **Final status:** Clean
- **Final issue counts:** High 0, Medium 0, Low 0

| Review | High | Medium | Low |
| --- | --- | --- | --- |
| Initial | 2 | 2 | 0 |
| Cycle 1 | 1 | 0 | 0 |
| Cycle 2 | 0 | 1 | 0 |
| Cycle 3 | 0 | 0 | 0 |
